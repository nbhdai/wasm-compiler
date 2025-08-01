use futures::{future::BoxFuture, Future, FutureExt, Stream, StreamExt};
use serde::Deserialize;
use snafu::prelude::*;
use std::{
    collections::{BTreeSet, HashMap},
    fmt, mem, ops,
    pin::pin,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::Duration,
};
use tokio::{
    process::{Child, ChildStdin, ChildStdout, Command},
    select,
    sync::{mpsc, oneshot, OnceCell},
    task::JoinSet,
    time::{self, MissedTickBehavior},
    try_join,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::{io::SyncIoBridge, sync::CancellationToken, task::AbortOnDropHandle};
use tracing::{error, info, info_span, instrument, trace, trace_span, warn, Instrument};

use crate::{
    bincode_input_closed,
    message::{
        CommandStatistics, CoordinatorMessage, DeleteFileRequest, ExecuteCommandRequest,
        ExecuteCommandResponse, JobId, Multiplexed, OneToOneResponse, ReadFileRequest,
        ReadFileResponse, SerializedError2, WorkerMessage, WriteFileRequest,
    },
    DropErrorDetailsExt, TaskAbortExt as _,
};

pub mod limits;

pub const PRIMARY_PATH: &'static str = "src/lib.rs"; 
pub const CARGO_TOML_KEY: &'static str = "cdylib";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelVersions {
    pub rustc: Version,
    pub rustfmt: Version,
    pub clippy: Version,
    pub miri: Option<Version>,
}

/// Parsing this struct is very lenient — we'd rather return some
/// partial data instead of absolutely nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub release: String,
    pub commit_hash: String,
    pub commit_date: String,
}

impl Version {
    fn parse_rustc_version_verbose(rustc_version: &str) -> Self {
        let mut release = "";
        let mut commit_hash = "";
        let mut commit_date = "";

        let fields = rustc_version.lines().skip(1).filter_map(|line| {
            let mut pieces = line.splitn(2, ':');
            let key = pieces.next()?.trim();
            let value = pieces.next()?.trim();
            Some((key, value))
        });

        for (k, v) in fields {
            match k {
                "release" => release = v,
                "commit-hash" => commit_hash = v,
                "commit-date" => commit_date = v,
                _ => {}
            }
        }

        Self {
            release: release.into(),
            commit_hash: commit_hash.into(),
            commit_date: commit_date.into(),
        }
    }

    // Parses versions of the shape `toolname 0.0.0 (0000000 0000-00-00)`
    fn parse_tool_version(tool_version: &str) -> Self {
        let mut parts = tool_version.split_whitespace().fuse().skip(1);

        let release = parts.next().unwrap_or("").into();
        let commit_hash = parts.next().unwrap_or("").trim_start_matches('(').into();
        let commit_date = parts.next().unwrap_or("").trim_end_matches(')').into();

        Self {
            release,
            commit_hash,
            commit_date,
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum VersionsError {
    #[snafu(display("Unable to determine versions for the stable channel"))]
    Stable { source: VersionsChannelError },

    #[snafu(display("Unable to determine versions for the beta channel"))]
    Beta { source: VersionsChannelError },

    #[snafu(display("Unable to determine versions for the nightly channel"))]
    Nightly { source: VersionsChannelError },
}

#[derive(Debug, Snafu)]
pub enum VersionsChannelError {
    #[snafu(transparent)]
    Channel { source: Error },

    #[snafu(transparent)]
    Versions { source: ContainerVersionsError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ContainerVersionsError {
    #[snafu(display("Failed to get `rustc` version"))]
    Rustc { source: VersionError },

    #[snafu(display("`rustc` not executable"))]
    RustcMissing,

    #[snafu(display("Failed to get `rustfmt` version"))]
    Rustfmt { source: VersionError },

    #[snafu(display("`cargo fmt` not executable"))]
    RustfmtMissing,

    #[snafu(display("Failed to get clippy version"))]
    Clippy { source: VersionError },

    #[snafu(display("`cargo clippy` not executable"))]
    ClippyMissing,

    #[snafu(display("Failed to get miri version"))]
    Miri { source: VersionError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum VersionError {
    #[snafu(display("Could not start the process"))]
    #[snafu(context(false))]
    SpawnProcess { source: SpawnCargoError },

    #[snafu(display("The task panicked"))]
    #[snafu(context(false))]
    TaskPanic { source: tokio::task::JoinError },
}

#[derive(Debug, Clone)]
pub struct Crate {
    pub name: String,
    pub version: String,
    pub id: String,
}

#[derive(Deserialize)]
struct InternalCrate {
    name: String,
    version: String,
    id: String,
}

impl From<InternalCrate> for Crate {
    fn from(other: InternalCrate) -> Self {
        let InternalCrate { name, version, id } = other;
        Self { name, version, id }
    }
}

#[derive(Debug, Snafu)]
pub enum CratesError {
    #[snafu(display("Could not start the container"))]
    #[snafu(context(false))]
    Start { source: Error },

    #[snafu(transparent)]
    Container { source: ContainerCratesError },
}

#[derive(Debug, Snafu)]
pub enum ContainerCratesError {
    #[snafu(display("Could not read the crate information file"))]
    #[snafu(context(false))]
    Read { source: CommanderError },

    #[snafu(display("Could not parse the crate information file"))]
    #[snafu(context(false))]
    Deserialization { source: serde_json::Error },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AliasingModel {
    Stacked,
    Tree,
}

#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub code: String,
}

impl CompileRequest {
    const OUTPUT_PATH: &str = "compilation";

    fn read_output_request(&self) -> ReadFileRequest {
        ReadFileRequest {
            path: Self::OUTPUT_PATH.to_owned(),
        }
    }
}

impl LowerRequest for CompileRequest {
    fn delete_previous_main_request(&self) -> DeleteFileRequest {
        delete_previous_primary_file_request()
    }

    fn write_main_request(&self) -> WriteFileRequest {
        write_primary_file_request(&self.code)
    }

    fn execute_cargo_request(&self) -> ExecuteCommandRequest {
        let mut args = vec!["wasm"];
        args.push("--release");
        args.extend(&["-o", Self::OUTPUT_PATH]);

        ExecuteCommandRequest {
            cmd: "cargo".to_owned(),
            args: args.into_iter().map(|s| s.to_owned()).collect(),
            envs: HashMap::new(),
            cwd: None,
        }
    }
}

impl CargoTomlModifier for CompileRequest {
    fn modify_cargo_toml(&self, mut cargo_toml: toml::Value) -> toml::Value {
        cargo_toml = modify_cargo_toml::set_crate_type(cargo_toml, CARGO_TOML_KEY);
        cargo_toml = modify_cargo_toml::set_release_lto(cargo_toml, true);

        cargo_toml
    }
}

#[derive(Debug, Clone)]
pub struct CompileResponse {
    pub success: bool,
    pub exit_detail: String,
    pub code: Vec<u8>,
}


#[derive(Debug, Clone)]
pub struct WithOutput<T> {
    pub response: T,
    pub stdout: String,
    pub stderr: String,
}

impl<T> WithOutput<T> {
    async fn try_absorb<F, E>(
        task: F,
        stdout_rx: mpsc::Receiver<String>,
        stderr_rx: mpsc::Receiver<String>,
    ) -> Result<WithOutput<T>, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        Self::try_absorb_stream(
            task,
            ReceiverStream::new(stdout_rx),
            ReceiverStream::new(stderr_rx),
        )
        .await
    }

    async fn try_absorb_stream<F, E>(
        task: F,
        stdout_rx: impl Stream<Item = String>,
        stderr_rx: impl Stream<Item = String>,
    ) -> Result<WithOutput<T>, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let stdout = stdout_rx.collect().map(Ok);
        let stderr = stderr_rx.collect().map(Ok);

        let (response, stdout, stderr) = try_join!(task, stdout, stderr)?;

        Ok(WithOutput {
            response,
            stdout,
            stderr,
        })
    }
}

impl<T> ops::Deref for WithOutput<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

fn write_primary_file_request(code: &str) -> WriteFileRequest {
    WriteFileRequest {
        path: PRIMARY_PATH.to_owned(),
        content: code.into(),
    }
}

fn delete_previous_primary_file_request() -> DeleteFileRequest {
    DeleteFileRequest {
        path: PRIMARY_PATH.to_owned(),
    }
}

#[derive(Debug)]
enum DemultiplexCommand {
    Listen(JobId, mpsc::Sender<WorkerMessage>),
    ListenOnce(JobId, oneshot::Sender<WorkerMessage>),
}

type ResourceError = Box<dyn snafu::Error + Send + Sync + 'static>;
type ResourceResult<T, E = ResourceError> = std::result::Result<T, E>;

/// Mediate resource limits and names for created objects.
///
/// The [`Coordinator`][] requires mostly-global resources, such as
/// Docker containers or running processes. This trait covers two cases:
///
/// 1. To avoid conflicts, each container must have a unique name.
/// 2. Containers and processes compete for CPU / memory.
///
/// Only a global view guarantees the unique names and resource
/// allocation, so whoever creates [`Coordinator`][]s via the
/// [`CoordinatorFactory`][] is responsible.
pub trait ResourceLimits: Send + Sync + fmt::Debug + 'static {
    /// Block until resources for a container are available.
    fn next_container(&self) -> BoxFuture<'static, ResourceResult<Box<dyn ContainerPermit>>>;

    /// Block until someone reqeusts that you return an in-use container.
    fn container_requested(&self) -> BoxFuture<'static, ()>;
}

/// Represents one allowed Docker container (or equivalent).
pub trait ContainerPermit: Send + Sync + fmt::Debug + fmt::Display + 'static {
    /// Block until resources for a process are available.
    fn next_process(&self) -> BoxFuture<'static, ResourceResult<Box<dyn ProcessPermit>>>;
}

/// Represents one allowed process.
pub trait ProcessPermit: Send + Sync + fmt::Debug + 'static {}

/// Enforces a limited number of concurrent `Coordinator`s.
#[derive(Debug)]
pub struct CoordinatorFactory {
    limits: Arc<dyn ResourceLimits>,
}

impl CoordinatorFactory {
    pub fn new(limits: Arc<dyn ResourceLimits>) -> Self {
        Self { limits }
    }

    pub fn build<B>(&self) -> Coordinator<B>
    where
        B: Backend + Default,
    {
        let limits = self.limits.clone();

        let backend = B::default();

        Coordinator::new(limits, backend)
    }

    pub async fn container_requested(&self) {
        self.limits.container_requested().await
    }
}

#[derive(Debug)]
pub struct Coordinator<B> {
    limits: Arc<dyn ResourceLimits>,
    backend: B,
    stable: OnceCell<Container>,
    token: CancelOnDrop,
}

/// Runs things.
///
/// # Liveness concerns
///
/// If you use one of the streaming versions (e.g. `begin_execute`),
/// you need to make sure that the stdout / stderr / status channels
/// are continuously read from or dropped completely. If not, one
/// channel can fill up, preventing the other channels from receiving
/// data as well.
impl<B> Coordinator<B>
where
    B: Backend,
{
    fn new(limits: Arc<dyn ResourceLimits>, backend: B) -> Self {
        let token = CancelOnDrop(CancellationToken::new());

        Self {
            limits,
            backend,
            stable: OnceCell::new(),
            token,
        }
    }

    pub async fn crates(&self) -> Result<Vec<Crate>, CratesError> {
        self.start_stable()
            .await?
            .crates()
            .await
            .map_err(Into::into)
    }

    pub async fn compile(
        &self,
        request: CompileRequest,
    ) -> Result<WithOutput<CompileResponse>, CompileError> {
        use compile_error::*;
        self.start_stable()
            .await
            .context(CouldNotStartContainerSnafu)?
            .compile(request)
            .await
    }

    pub async fn begin_compile(
        &self,
        token: CancellationToken,
        request: CompileRequest,
    ) -> Result<ActiveCompilation, CompileError> {
        use compile_error::*;
        self.start_stable()
            .await
            .context(CouldNotStartContainerSnafu)?
            .begin_compile(token, request)
            .await
    }

    pub async fn idle(&mut self) -> Result<()> {
        let token = mem::take(&mut self.token);
        token.cancel();
        match self.stable.take() {
            Some(c) => c.shutdown().await,
            _ => Ok(()),
        }?;

        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<B> {
        self.idle().await?;
        Ok(self.backend)
    }

    async fn start_stable(&self) -> Result<&Container, Error> {
        self.stable
            .get_or_try_init(|| {
                let limits = self.limits.clone();
                let token = self.token.0.clone();
                Container::new(limits, token, &self.backend)
            })
            .await
    }
}

#[derive(Debug, Default)]
struct CancelOnDrop(CancellationToken);

impl CancelOnDrop {
    fn cancel(&self) {
        self.0.cancel();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Debug)]
struct Container {
    permit: Box<dyn ContainerPermit>,
    task: AbortOnDropHandle<Result<()>>,
    kill_child: TerminateContainer,
    modify_cargo_toml: ModifyCargoToml,
    commander: Commander,
}

impl Container {
    async fn new(
        limits: Arc<dyn ResourceLimits>,
        token: CancellationToken,
        backend: &impl Backend,
    ) -> Result<Self> {
        let permit = limits.next_container().await.context(AcquirePermitSnafu)?;

        let (mut child, kill_child, stdin, stdout) =
            backend.run_worker_in_background(&permit)?;
        let IoQueue {
            mut tasks,
            to_worker_tx,
            from_worker_rx,
        } = spawn_io_queue(stdin, stdout, token);

        let (command_tx, command_rx) = mpsc::channel(8);
        let demultiplex = Commander::demultiplex(command_rx, from_worker_rx);
        let demultiplex_task = tokio::spawn(demultiplex.in_current_span()).abort_on_drop();

        let task = tokio::spawn(
            async move {
                let child = async {
                    match child.wait().await.context(JoinWorkerSnafu) {
                        Ok(exit_status) => {
                            tracing::debug!(?exit_status, "Container Creation Exit");
                            Ok(())
                        }
                        Err(error) => {
                            tracing::error!(?error, "Container Creation Exit");
                            Ok(())

                        },
                    }
                };

                let demultiplex_task = async {
                    demultiplex_task
                        .await
                        .context(DemultiplexerTaskPanickedSnafu)?
                        .context(DemultiplexerTaskFailedSnafu)
                };

                let task = async {
                    if let Some(t) = tasks.join_next().await {
                        t.context(IoQueuePanickedSnafu)??;
                    }
                    Ok(())
                };

                let (c, d, t) = try_join!(child, demultiplex_task, task)?;
                let _: [(); 3] = [c, d, t];

                Ok(())
            }
            .in_current_span(),
        )
        .abort_on_drop();

        let commander = Commander {
            to_worker_tx,
            to_demultiplexer_tx: command_tx,
            id: Default::default(),
        };

        let modify_cargo_toml = ModifyCargoToml::new(commander.clone())
            .await
            .context(CouldNotLoadCargoTomlSnafu)?;

        Ok(Container {
            permit,
            task,
            kill_child,
            modify_cargo_toml,
            commander,
        })
    }

    async fn crates(&self) -> Result<Vec<Crate>, ContainerCratesError> {
        let read = ReadFileRequest {
            path: "crate-information.json".into(),
        };
        let read = self.commander.one(read).await?;
        let crates = serde_json::from_slice::<Vec<InternalCrate>>(&read.0)?;
        Ok(crates.into_iter().map(Into::into).collect())
    }

    async fn compile(
        &self,
        request: CompileRequest,
    ) -> Result<WithOutput<CompileResponse>, CompileError> {
        let token = Default::default();

        let ActiveCompilation {
            permit: _permit,
            task,
            stdout_rx,
            stderr_rx,
        } = self.begin_compile(token, request).await?;

        WithOutput::try_absorb(task, stdout_rx, stderr_rx).await
    }

    #[instrument(skip_all)]
    async fn begin_compile(
        &self,
        token: CancellationToken,
        request: CompileRequest,
    ) -> Result<ActiveCompilation, CompileError> {
        use compile_error::*;
        tracing::debug!("Starting compilation");

        let SpawnCargo {
            permit,
            task,
            stdin_tx,
            stdout_rx,
            stderr_rx,
            status_rx,
        } = self.do_request(&request, token).await?;

        drop(stdin_tx);
        drop(status_rx);

        let commander = self.commander.clone();
        let task = async move {
            let ExecuteCommandResponse {
                success,
                exit_detail,
            } = task
                .await
                .context(CargoTaskPanickedSnafu)?
                .context(CargoFailedSnafu)?;

            let code = if success {
                let read_output = request.read_output_request();

                let file: ReadFileResponse = commander
                    .one(read_output)
                    .await
                    .context(CouldNotReadCodeSnafu)?;
                file.0
            } else {
                Vec::new()
            };

            Ok(CompileResponse {
                success,
                exit_detail,
                code,
            })
        }
        .boxed();

        Ok(ActiveCompilation {
            permit,
            task,
            stdout_rx,
            stderr_rx,
        })
    }

    async fn do_request(
        &self,
        request: impl LowerRequest + CargoTomlModifier,
        token: CancellationToken,
    ) -> Result<SpawnCargo, DoRequestError> {
        use do_request_error::*;

        let delete_previous_main = async {
            self.commander
                .one(request.delete_previous_main_request())
                .await
                .context(CouldNotDeletePreviousCodeSnafu)
                .map(drop::<crate::message::DeleteFileResponse>)
        };

        let write_main = async {
            self.commander
                .one(request.write_main_request())
                .await
                .context(CouldNotWriteCodeSnafu)
                .map(drop::<crate::message::WriteFileResponse>)
        };

        let modify_cargo_toml = async {
            self.modify_cargo_toml
                .modify_for(&request)
                .await
                .context(CouldNotModifyCargoTomlSnafu)
        };

        let (d, w, m) = try_join!(delete_previous_main, write_main, modify_cargo_toml)?;
        let _: [(); 3] = [d, w, m];

        let execute_cargo = request.execute_cargo_request();
        self.spawn_cargo_task(token, execute_cargo)
            .await
            .context(CouldNotStartCargoSnafu)
    }

    async fn spawn_cargo_task(
        &self,
        token: CancellationToken,
        execute_cargo: ExecuteCommandRequest,
    ) -> Result<SpawnCargo, SpawnCargoError> {
        use spawn_cargo_error::*;

        let permit = self
            .permit
            .next_process()
            .await
            .context(AcquirePermitSnafu)?;

        trace!(?execute_cargo, "starting cargo task");

        let (stdin_tx, mut stdin_rx) = mpsc::channel(8);
        let (stdout_tx, stdout_rx) = mpsc::channel(8);
        let (stderr_tx, stderr_rx) = mpsc::channel(8);
        let (status_tx, status_rx) = mpsc::channel(8);

        let (to_worker_tx, mut from_worker_rx) = self
            .commander
            .many(execute_cargo)
            .await
            .context(CouldNotStartCargoSnafu)?;

        let task = tokio::spawn({
            async move {
                let mut cancelled = pin!(token.cancelled().fuse());
                let mut stdin_open = true;

                loop {
                    enum Event {
                        Cancelled,
                        Stdin(Option<String>),
                        FromWorker(WorkerMessage),
                    }
                    use Event::*;

                    let event = select! {
                        () = &mut cancelled => Cancelled,

                        stdin = stdin_rx.recv(), if stdin_open => Stdin(stdin),

                        Some(container_msg) = from_worker_rx.recv() => FromWorker(container_msg),

                        else => return UnexpectedEndOfMessagesSnafu.fail(),
                    };

                    match event {
                        Cancelled => {
                            let msg = CoordinatorMessage::Kill;
                            trace!(msg_name = msg.as_ref(), "processing");
                            to_worker_tx.send(msg).await.context(KillSnafu)?;
                        }

                        Stdin(stdin) => {
                            let msg = match stdin {
                                Some(stdin) => CoordinatorMessage::StdinPacket(stdin),

                                None => {
                                    stdin_open = false;
                                    CoordinatorMessage::StdinClose
                                }
                            };

                            trace!(msg_name = msg.as_ref(), "processing");
                            to_worker_tx.send(msg).await.context(StdinSnafu)?;
                        }

                        FromWorker(container_msg) => {
                            trace!(msg_name = container_msg.as_ref(), "processing");

                            match container_msg {
                                WorkerMessage::ExecuteCommand(resp) => {
                                    return Ok(resp);
                                }

                                WorkerMessage::StdoutPacket(packet) => {
                                    stdout_tx.send(packet).await.ok(/* Receiver gone, that's OK */);
                                }

                                WorkerMessage::StderrPacket(packet) => {
                                    stderr_tx.send(packet).await.ok(/* Receiver gone, that's OK */);
                                }

                                WorkerMessage::CommandStatistics(stats) => {
                                    status_tx.send(stats).await.ok(/* Receiver gone, that's OK */);
                                }

                                WorkerMessage::Error(e) => {
                                    return Err(SerializedError2::adapt(e)).context(WorkerSnafu);
                                }

                                WorkerMessage::Error2(e) => return Err(e).context(WorkerSnafu),

                                _ => {
                                    let message = container_msg.as_ref();
                                    return UnexpectedMessageSnafu { message }.fail();
                                }
                            }
                        }
                    }
                }
            }
            .instrument(trace_span!("cargo task").or_current())
        })
        .abort_on_drop();

        Ok(SpawnCargo {
            permit,
            task,
            stdin_tx,
            stdout_rx,
            stderr_rx,
            status_rx,
        })
    }

    async fn shutdown(self) -> Result<()> {
        let Self {
            permit,
            task,
            mut kill_child,
            modify_cargo_toml,
            commander,
        } = self;
        drop(commander);
        drop(modify_cargo_toml);

        kill_child.terminate_now().await?;

        let r = task.await;
        drop(permit);

        r.context(ContainerTaskPanickedSnafu)?
    }
}



pub struct ActiveCompilation {
    pub permit: Box<dyn ProcessPermit>,
    pub task: BoxFuture<'static, Result<CompileResponse, CompileError>>,
    pub stdout_rx: mpsc::Receiver<String>,
    pub stderr_rx: mpsc::Receiver<String>,
}

impl fmt::Debug for ActiveCompilation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActiveCompilation")
            .field("task", &"<future>")
            .field("stdout_rx", &self.stdout_rx)
            .field("stderr_rx", &self.stderr_rx)
            .finish()
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum CompileError {
    #[snafu(display("Could not start the container"))]
    CouldNotStartContainer { source: Error },

    #[snafu(transparent)]
    DoRequest { source: DoRequestError },

    #[snafu(display("The Cargo task panicked"))]
    CargoTaskPanicked { source: tokio::task::JoinError },

    #[snafu(display("Cargo task failed"))]
    CargoFailed { source: SpawnCargoError },

    #[snafu(display("Could not read the compilation output"))]
    CouldNotReadCode { source: CommanderError },

    #[snafu(display("The compilation output was not UTF-8"))]
    CodeNotUtf8 { source: std::string::FromUtf8Error },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum DoRequestError {
    #[snafu(display("Could not modify Cargo.toml"))]
    CouldNotModifyCargoToml { source: ModifyCargoTomlError },

    #[snafu(display("Could not delete previous source code"))]
    CouldNotDeletePreviousCode { source: CommanderError },

    #[snafu(display("Could not write source code"))]
    CouldNotWriteCode { source: CommanderError },

    #[snafu(display("Could not start Cargo task"))]
    CouldNotStartCargo { source: SpawnCargoError },
}

struct SpawnCargo {
    permit: Box<dyn ProcessPermit>,
    task: AbortOnDropHandle<Result<ExecuteCommandResponse, SpawnCargoError>>,
    stdin_tx: mpsc::Sender<String>,
    stdout_rx: mpsc::Receiver<String>,
    stderr_rx: mpsc::Receiver<String>,
    status_rx: mpsc::Receiver<CommandStatistics>,
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum SpawnCargoError {
    #[snafu(display("Could not start Cargo"))]
    CouldNotStartCargo { source: CommanderError },

    #[snafu(display("The worker operation failed"))]
    Worker { source: SerializedError2 },

    #[snafu(display("Received the unexpected message `{message}`"))]
    UnexpectedMessage { message: String },

    #[snafu(display("There are no more messages"))]
    UnexpectedEndOfMessages,

    #[snafu(display("Unable to send stdin message"))]
    Stdin { source: MultiplexedSenderError },

    #[snafu(display("Unable to send kill message"))]
    Kill { source: MultiplexedSenderError },

    #[snafu(display("Could not acquire a process permit"))]
    AcquirePermit { source: ResourceError },
}

#[derive(Debug, Clone)]
struct Commander {
    to_worker_tx: mpsc::Sender<Multiplexed<CoordinatorMessage>>,
    to_demultiplexer_tx: mpsc::Sender<(oneshot::Sender<()>, DemultiplexCommand)>,
    id: Arc<AtomicU64>,
}

trait LowerRequest {
    fn delete_previous_main_request(&self) -> DeleteFileRequest;

    fn write_main_request(&self) -> WriteFileRequest;

    fn execute_cargo_request(&self) -> ExecuteCommandRequest;
}

impl<S> LowerRequest for &S
where
    S: LowerRequest,
{
    fn delete_previous_main_request(&self) -> DeleteFileRequest {
        S::delete_previous_main_request(self)
    }

    fn write_main_request(&self) -> WriteFileRequest {
        S::write_main_request(self)
    }

    fn execute_cargo_request(&self) -> ExecuteCommandRequest {
        S::execute_cargo_request(self)
    }
}

trait CargoTomlModifier {
    fn modify_cargo_toml(&self, cargo_toml: toml::Value) -> toml::Value;
}

impl<C> CargoTomlModifier for &C
where
    C: CargoTomlModifier,
{
    fn modify_cargo_toml(&self, cargo_toml: toml::Value) -> toml::Value {
        C::modify_cargo_toml(self, cargo_toml)
    }
}

#[derive(Debug)]
struct ModifyCargoToml {
    commander: Commander,
    cargo_toml: toml::Value,
}

impl ModifyCargoToml {
    const PATH: &'static str = "Cargo.toml";

    async fn new(commander: Commander) -> Result<Self, ModifyCargoTomlError> {
        let cargo_toml = Self::read(&commander).await?;
        Ok(Self {
            commander,
            cargo_toml,
        })
    }

    async fn modify_for(
        &self,
        request: &impl CargoTomlModifier,
    ) -> Result<(), ModifyCargoTomlError> {
        let cargo_toml = self.cargo_toml.clone();
        let cargo_toml = request.modify_cargo_toml(cargo_toml);
        Self::write(&self.commander, cargo_toml).await
    }

    async fn read(commander: &Commander) -> Result<toml::Value, ModifyCargoTomlError> {
        use modify_cargo_toml_error::*;

        let path = Self::PATH.to_owned();
        let cargo_toml = commander
            .one(ReadFileRequest { path })
            .await
            .context(CouldNotReadSnafu)?;

        let cargo_toml = String::from_utf8(cargo_toml.0)?;
        let cargo_toml = toml::from_str(&cargo_toml)?;

        Ok(cargo_toml)
    }

    async fn write(
        commander: &Commander,
        cargo_toml: toml::Value,
    ) -> Result<(), ModifyCargoTomlError> {
        use modify_cargo_toml_error::*;

        let cargo_toml = toml::to_string(&cargo_toml)?;
        let content = cargo_toml.into_bytes();

        let path = Self::PATH.to_owned();
        commander
            .one(WriteFileRequest { path, content })
            .await
            .context(CouldNotWriteSnafu)?;

        Ok(())
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ModifyCargoTomlError {
    #[snafu(display("Could not read the file"))]
    CouldNotRead { source: CommanderError },

    #[snafu(display("Could not parse the file as UTF-8"))]
    #[snafu(context(false))]
    InvalidUtf8 { source: std::string::FromUtf8Error },

    #[snafu(display("Could not deserialize the file as TOML"))]
    #[snafu(context(false))]
    CouldNotDeserialize { source: toml::de::Error },

    #[snafu(display("Could not serialize the file as TOML"))]
    #[snafu(context(false))]
    CouldNotSerialize { source: toml::ser::Error },

    #[snafu(display("Could not write the file"))]
    CouldNotWrite { source: CommanderError },
}

struct MultiplexedSender {
    job_id: JobId,
    to_worker_tx: mpsc::Sender<Multiplexed<CoordinatorMessage>>,
}

impl MultiplexedSender {
    async fn send(
        &self,
        message: impl Into<CoordinatorMessage>,
    ) -> Result<(), MultiplexedSenderError> {
        use multiplexed_sender_error::*;

        let message = message.into();
        let message = Multiplexed(self.job_id, message);

        self.to_worker_tx
            .send(message)
            .await
            .drop_error_details()
            .context(MultiplexedSenderSnafu)
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
#[snafu(display("Could not send a message to the worker"))]
pub struct MultiplexedSenderError {
    source: mpsc::error::SendError<()>,
}

impl Commander {
    const GC_PERIOD: Duration = Duration::from_secs(30);

    #[instrument(skip_all)]
    async fn demultiplex(
        mut command_rx: mpsc::Receiver<(oneshot::Sender<()>, DemultiplexCommand)>,
        mut from_worker_rx: mpsc::Receiver<Multiplexed<WorkerMessage>>,
    ) -> Result<(), CommanderError> {
        use commander_error::*;

        let mut waiting = HashMap::new();
        let mut waiting_once = HashMap::new();

        let mut gc_interval = time::interval(Self::GC_PERIOD);
        gc_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            enum Event {
                Command(Option<(oneshot::Sender<()>, DemultiplexCommand)>),

                FromWorker(Option<Multiplexed<WorkerMessage>>),

                // Find any channels where the receivers have been
                // dropped and clear out the sending halves.
                Gc,
            }
            use Event::*;

            let event = select! {
                command = command_rx.recv() => Command(command),

                msg = from_worker_rx.recv() => FromWorker(msg),

                _ = gc_interval.tick() => Gc,
            };

            match event {
                Command(None) => break,
                Command(Some((ack_tx, command))) => {
                    match command {
                        DemultiplexCommand::Listen(job_id, waiter) => {
                            trace!(job_id, "adding listener (many)");
                            let old = waiting.insert(job_id, waiter);
                            ensure!(old.is_none(), DuplicateDemultiplexerClientSnafu { job_id });
                        }

                        DemultiplexCommand::ListenOnce(job_id, waiter) => {
                            trace!(job_id, "adding listener (once)");
                            let old = waiting_once.insert(job_id, waiter);
                            ensure!(old.is_none(), DuplicateDemultiplexerClientSnafu { job_id });
                        }
                    }

                    ack_tx.send(()).ok(/* Don't care about it */);
                }

                FromWorker(None) => break,
                FromWorker(Some(Multiplexed(job_id, msg))) => {
                    if let Some(waiter) = waiting_once.remove(&job_id) {
                        trace!(job_id, "notifying listener (once)");
                        waiter.send(msg).ok(/* Don't care about it */);
                        continue;
                    }

                    if let Some(waiter) = waiting.get(&job_id) {
                        trace!(job_id, "notifying listener (many)");
                        waiter.send(msg).await.ok(/* Don't care about it */);
                        continue;
                    }

                    warn!(job_id, msg_name = msg.as_ref(), "no listener to notify");
                }

                Gc => {
                    waiting.retain(|_job_id, tx| !tx.is_closed());
                    waiting_once.retain(|_job_id, tx| !tx.is_closed());
                }
            }
        }

        Ok(())
    }

    fn next_id(&self) -> JobId {
        self.id.fetch_add(1, Ordering::SeqCst)
    }

    async fn send_to_demultiplexer(
        &self,
        command: DemultiplexCommand,
    ) -> Result<(), CommanderError> {
        use commander_error::*;

        let (ack_tx, ack_rx) = oneshot::channel();

        self.to_demultiplexer_tx
            .send((ack_tx, command))
            .await
            .drop_error_details()
            .context(UnableToSendToDemultiplexerSnafu)?;

        ack_rx.await.context(DemultiplexerDidNotRespondSnafu)
    }

    fn build_multiplexed_sender(&self, job_id: JobId) -> MultiplexedSender {
        let to_worker_tx = self.to_worker_tx.clone();
        MultiplexedSender {
            job_id,
            to_worker_tx,
        }
    }

    async fn one<M>(&self, message: M) -> Result<M::Response, CommanderError>
    where
        M: Into<CoordinatorMessage>,
        M: OneToOneResponse,
        Result<M::Response, SerializedError2>: TryFrom<WorkerMessage>,
    {
        use commander_error::*;

        let id = self.next_id();
        let to_worker_tx = self.build_multiplexed_sender(id);
        let (from_demultiplexer_tx, from_demultiplexer_rx) = oneshot::channel();

        self.send_to_demultiplexer(DemultiplexCommand::ListenOnce(id, from_demultiplexer_tx))
            .await?;
        to_worker_tx
            .send(message)
            .await
            .context(UnableToStartOneSnafu)?;
        let msg = from_demultiplexer_rx
            .await
            .context(UnableToReceiveFromDemultiplexerSnafu)?;

        match <Result<_, _>>::try_from(msg) {
            Ok(v) => v.context(WorkerOperationFailedSnafu),
            Err(_) => UnexpectedResponseTypeSnafu.fail(),
        }
    }

    async fn many<M>(
        &self,
        message: M,
    ) -> Result<(MultiplexedSender, mpsc::Receiver<WorkerMessage>), CommanderError>
    where
        M: Into<CoordinatorMessage>,
    {
        use commander_error::*;

        let id = self.next_id();
        let to_worker_tx = self.build_multiplexed_sender(id);
        let (from_worker_tx, from_worker_rx) = mpsc::channel(8);

        self.send_to_demultiplexer(DemultiplexCommand::Listen(id, from_worker_tx))
            .await?;
        to_worker_tx
            .send(message)
            .await
            .context(UnableToStartManySnafu)?;

        Ok((to_worker_tx, from_worker_rx))
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum CommanderError {
    #[snafu(display("Two listeners subscribed to job {job_id}"))]
    DuplicateDemultiplexerClient { job_id: JobId },

    #[snafu(display("Could not send a message to the demultiplexer"))]
    UnableToSendToDemultiplexer { source: mpsc::error::SendError<()> },

    #[snafu(display("Could not send a message to the demultiplexer"))]
    DemultiplexerDidNotRespond { source: oneshot::error::RecvError },

    #[snafu(display("Did not receive a response from the demultiplexer"))]
    UnableToReceiveFromDemultiplexer { source: oneshot::error::RecvError },

    #[snafu(display("Could not start single request/response interaction"))]
    UnableToStartOne { source: MultiplexedSenderError },

    #[snafu(display("Could not start continuous interaction"))]
    UnableToStartMany { source: MultiplexedSenderError },

    #[snafu(display("Did not receive the expected response type from the worker"))]
    UnexpectedResponseType,

    #[snafu(display("The worker operation failed"))]
    WorkerOperationFailed { source: SerializedError2 },
}

pub static TRACKED_CONTAINERS: LazyLock<Mutex<BTreeSet<Arc<str>>>> =
    LazyLock::new(Default::default);

#[derive(Debug)]
pub struct TerminateContainer(Option<(String, Command)>);

impl TerminateContainer {
    pub fn new(name: String, command: Command) -> Self {
        Self::start_tracking(&name);

        Self(Some((name, command)))
    }

    pub fn none() -> Self {
        Self(None)
    }

    async fn terminate_now(&mut self) -> Result<(), TerminateContainerError> {
        use terminate_container_error::*;

        if let Some((name, mut kill_child)) = self.0.take() {
            Self::stop_tracking(&name);
            let o = kill_child
                .output()
                .await
                .context(TerminateContainerSnafu { name: &name })?;
            Self::report_failure(name, o);
        }

        Ok(())
    }

    #[instrument]
    fn start_tracking(name: &str) {
        let was_inserted = TRACKED_CONTAINERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into());

        if was_inserted {
            info!("Started tracking container");
        } else {
            error!("Started tracking container, but it was already tracked");
        }
    }

    #[instrument]
    fn stop_tracking(name: &str) {
        let was_tracked = TRACKED_CONTAINERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);

        if was_tracked {
            info!("Stopped tracking container");
        } else {
            error!("Stopped tracking container, but it was not in the tracking set");
        }
    }

    fn report_failure(name: String, s: std::process::Output) {
        // We generally don't care if the command itself succeeds or
        // not; the container may already be dead! However, let's log
        // it in an attempt to debug cases where there are more
        // containers running than we expect.

        if !s.status.success() {
            let code = s.status.code();
            // FUTURE: use `_owned`
            let stdout = String::from_utf8_lossy(&s.stdout);
            let stderr = String::from_utf8_lossy(&s.stderr);

            let stdout = stdout.trim();
            let stderr = stderr.trim();

            error!(?code, %stdout, %stderr, %name, "Killing the container failed");
        }
    }
}

impl Drop for TerminateContainer {
    fn drop(&mut self) {
        if let Some((name, mut kill_child)) = self.0.take() {
            Self::stop_tracking(&name);
            match kill_child.as_std_mut().output() {
                Ok(o) => Self::report_failure(name, o),
                Err(e) => error!("Unable to kill container {name} while dropping: {e}"),
            }
        }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
#[snafu(display("Unable to kill the Docker container {name}"))]
pub struct TerminateContainerError {
    name: String,
    source: std::io::Error,
}

pub trait Backend {
    fn run_worker_in_background(
        &self,
        id: impl fmt::Display,
    ) -> Result<(Child, TerminateContainer, ChildStdin, ChildStdout)> {
        let (mut start, kill) = self.prepare_worker_command(id);

        let mut child = start
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context(SpawnWorkerSnafu)?;
        let stdin = child.stdin.take().context(WorkerStdinCaptureSnafu)?;
        let stdout = child.stdout.take().context(WorkerStdoutCaptureSnafu)?;
        Ok((child, kill, stdin, stdout))
    }

    fn prepare_worker_command(
        &self,
        id: impl fmt::Display,
    ) -> (Command, TerminateContainer);
}

impl<B> Backend for &B
where
    B: Backend,
{
    fn prepare_worker_command(
        &self,
        id: impl fmt::Display,
    ) -> (Command, TerminateContainer) {
        B::prepare_worker_command(self, id)
    }
}

macro_rules! docker_command {
    ($($arg:expr),* $(,)?) => ({
        let mut cmd = Command::new("podman");
        $( cmd.arg($arg); )*
        cmd
    });
}

macro_rules! docker_target_arch {
    (x86_64: $x:expr, aarch64: $a:expr $(,)?) => {{
        #[cfg(target_arch = "x86_64")]
        {
            $x
        }

        #[cfg(target_arch = "aarch64")]
        {
            $a
        }
    }};
}

const DOCKER_ARCH: &str = docker_target_arch! {
    x86_64: "linux/amd64",
    aarch64: "linux/arm64",
};

fn basic_secure_docker_command() -> Command {
    docker_command!(
        "run",
        "--platform",
        DOCKER_ARCH,
        "--cap-drop=ALL",
        "--net",
        "none",
        "--memory",
        "1g",
        "--memory-swap",
        "2g",
        "--pids-limit",
        "512",
        "--oom-score-adj",
        "1000",
    )
}

#[derive(Default)]
pub struct DockerBackend(());

impl Backend for DockerBackend {
    fn prepare_worker_command(
        &self,
        id: impl fmt::Display,
    ) -> (Command, TerminateContainer) {
        let name = format!("wasmbuild-{id}");

        let mut command = basic_secure_docker_command();
        command
            .args(["--name", &name])
            .arg("-i")
            .args(["-a", "stdin", "-a", "stdout", "-a", "stderr"])
            .arg("--rm")
            .arg("rust-stable")
            .arg("worker")
            .arg("/wasmbuild");

        let mut kill = Command::new("docker");
        kill.arg("kill").args(["--signal", "KILL"]).arg(&name);
        let kill = TerminateContainer::new(name, kill);
        tracing::debug!("prepared Docker Command: {:?}", command);
        (command, kill)
    }
}


pub type Result<T, E = Error> = ::std::result::Result<T, E>;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Reached system process limit"))]
    SpawnWorker { source: std::io::Error },

    #[snafu(display("Unable to join child process"))]
    JoinWorker { source: std::io::Error },

    #[snafu(display("The demultiplexer task panicked"))]
    DemultiplexerTaskPanicked { source: tokio::task::JoinError },

    #[snafu(display("The demultiplexer task failed"))]
    DemultiplexerTaskFailed { source: CommanderError },

    #[snafu(display("The IO queue task panicked"))]
    IoQueuePanicked { source: tokio::task::JoinError },

    #[snafu(transparent)]
    KillWorker { source: TerminateContainerError },

    #[snafu(display("The container task panicked"))]
    ContainerTaskPanicked { source: tokio::task::JoinError },

    #[snafu(display("Worker process's stdin not captured"))]
    WorkerStdinCapture,

    #[snafu(display("Worker process's stdout not captured"))]
    WorkerStdoutCapture,

    #[snafu(display("Failed to flush child stdin"))]
    WorkerStdinFlush { source: std::io::Error },

    #[snafu(display("Failed to deserialize worker message"))]
    WorkerMessageDeserialization { source: bincode::Error },

    #[snafu(display("Failed to serialize coordinator message"))]
    CoordinatorMessageSerialization { source: bincode::Error },

    #[snafu(display("Failed to send worker message through channel"))]
    UnableToSendWorkerMessage { source: mpsc::error::SendError<()> },

    #[snafu(display("Unable to load original Cargo.toml"))]
    CouldNotLoadCargoToml { source: ModifyCargoTomlError },

    #[snafu(display("Could not acquire a container permit"))]
    AcquirePermit { source: ResourceError },
}

struct IoQueue {
    tasks: JoinSet<Result<()>>,
    to_worker_tx: mpsc::Sender<Multiplexed<CoordinatorMessage>>,
    from_worker_rx: mpsc::Receiver<Multiplexed<WorkerMessage>>,
}

// Child stdin/out <--> messages.
fn spawn_io_queue(stdin: ChildStdin, stdout: ChildStdout, token: CancellationToken) -> IoQueue {
    use std::io::{prelude::*, BufReader, BufWriter};

    let mut tasks = JoinSet::new();

    let (tx, from_worker_rx) = mpsc::channel(8);
    tasks.spawn_blocking(move || {
        let span = info_span!("child_io_queue::input");
        let _span = span.enter();

        let stdout = SyncIoBridge::new(stdout);
        let mut stdout = BufReader::new(stdout);

        loop {
            let worker_msg = bincode::deserialize_from(&mut stdout);

            if bincode_input_closed(&worker_msg) {
                break;
            };

            let worker_msg = worker_msg.context(WorkerMessageDeserializationSnafu)?;

            tx.blocking_send(worker_msg)
                .drop_error_details()
                .context(UnableToSendWorkerMessageSnafu)?;
        }

        Ok(())
    });

    let (to_worker_tx, mut rx) = mpsc::channel(8);
    tasks.spawn_blocking(move || {
        let span = info_span!("child_io_queue::output");
        let _span = span.enter();

        let stdin = SyncIoBridge::new(stdin);
        let mut stdin = BufWriter::new(stdin);

        let handle = tokio::runtime::Handle::current();

        loop {
            let coordinator_msg = handle.block_on(token.run_until_cancelled(rx.recv()));

            let Some(Some(coordinator_msg)) = coordinator_msg else {
                break;
            };

            bincode::serialize_into(&mut stdin, &coordinator_msg)
                .context(CoordinatorMessageSerializationSnafu)?;

            stdin.flush().context(WorkerStdinFlushSnafu)?;
        }

        Ok(())
    });

    IoQueue {
        tasks,
        to_worker_tx,
        from_worker_rx,
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use futures::future::{join, try_join_all};
    use std::{
        env,
        sync::{LazyLock, Once},
    };
    use tempfile::TempDir;

    use super::*;

    #[allow(dead_code)]
    fn setup_tracing() {
        use tracing::Level;
        use tracing_subscriber::fmt::TestWriter;

        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(Level::TRACE)
            .with_writer(TestWriter::new())
            .try_init()
            .ok();
    }

    #[derive(Debug)]
    struct TestBackend {
        project_dir: TempDir,
    }

    impl Default for TestBackend {
        fn default() -> Self {
            static COMPILE_WORKER_ONCE: Once = Once::new();

            COMPILE_WORKER_ONCE.call_once(|| {
                let output = std::process::Command::new("cargo")
                    .arg("build")
                    .output()
                    .expect("Build failed");
                assert!(output.status.success(), "Build failed");
            });

            let project_dir = TempDir::with_prefix("nbhdai")
                .expect("Failed to create temporary project directory");

            let channel = "rust-stable";
            let channel_dir = project_dir.path().join(channel);

            let output = std::process::Command::new("cargo")
                .arg(format!("+{channel}"))
                .arg("new")
                .args(["--name", "nbhdai"])
                .arg(&channel_dir)
                .output()
                .expect("Cargo new failed");
            assert!(output.status.success(), "Cargo new failed");

            let main = channel_dir.join("src").join("main.rs");
            std::fs::remove_file(main).expect("Could not delete main.rs");

            Self { project_dir }
        }
    }

    impl Backend for TestBackend {
        fn prepare_worker_command(
            &self,
            _id: impl fmt::Display,
        ) -> (Command, TerminateContainer) {
            let channel_dir = self.project_dir.path();

            let mut command = Command::new("./target/debug/worker");
            command.env("RUSTUP_TOOLCHAIN", "rust-stable");
            command.arg(channel_dir);

            (command, TerminateContainer::none())
        }
    }

    static MAX_CONCURRENT_TESTS: LazyLock<usize> = LazyLock::new(|| {
        env::var("TESTS_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5)
    });

    static TEST_COORDINATOR_ID_PROVIDER: LazyLock<Arc<limits::Global>> =
        LazyLock::new(|| Arc::new(limits::Global::new(100, *MAX_CONCURRENT_TESTS)));

    static TEST_COORDINATOR_FACTORY: LazyLock<CoordinatorFactory> =
        LazyLock::new(|| CoordinatorFactory::new(TEST_COORDINATOR_ID_PROVIDER.clone()));

    fn new_coordinator_test() -> Coordinator<TestBackend> {
        TEST_COORDINATOR_FACTORY.build()
    }

    fn new_coordinator_docker() -> Coordinator<DockerBackend> {
        TEST_COORDINATOR_FACTORY.build()
    }

    fn new_coordinator() -> Coordinator<impl Backend> {
        #[cfg(not(force_docker))]
        {
            new_coordinator_test()
        }

        #[cfg(force_docker)]
        {
            new_coordinator_docker()
        }
    }


    #[tokio::test]
    #[snafu::report]
    async fn compile_wasm() -> Result<()> {
        // cargo-wasm only exists inside the container
        let coordinator = new_coordinator_docker();

        let req = CompileRequest {
            code: r#"#[export_name = "inc"] pub fn inc(a: u8) -> u8 { a + 1 }"#.into(),
        };

        let response = coordinator.compile(req).with_timeout().await.unwrap();
        assert!(response.success, "stderr: {}", response.stderr);
        assert!(
            !response.code.is_empty(),
            "No response"
        );

        coordinator.shutdown().await?;

        Ok(())
    }

    #[tokio::test]
    #[snafu::report]
    async fn compile_base_wasm() -> Result<()> {
        // cargo-wasm only exists inside the container
        let coordinator = new_coordinator();

        let req = CompileRequest {
            code: r#"#[export_name = "inc"] pub fn inc(a: u8) -> u8 { a + 1 }"#.into(),
        };

        let response = coordinator.compile(req).with_timeout().await.unwrap();
        assert!(response.success, "stderr: {}", response.stderr);
        assert!(
            !response.code.is_empty(),
            "No response"
        );

        coordinator.shutdown().await?;

        Ok(())
    }

    // The next set of tests are broader than the functionality of a
    // single operation.

    #[tokio::test]
    #[snafu::report]
    async fn compile_clears_old_lib_rs() -> Result<()> {
        let coordinator = new_coordinator();

        let req = CompileRequest {
            code: r#"pub fn alpha(a: u8) -> u8 { a + 1 }"#.into(),
        };

        let response = coordinator
            .compile(req.clone())
            .with_timeout()
            .await
            .unwrap();
        assert!(!response.success, "stderr: {}", response.stderr);
        assert_contains!(response.stderr, "`main` function not found");

        
        let req = CompileRequest {
            code: r#"pub fn beta(a: u8) -> u8 { a + 1 }"#.into(),
        };

        let response = coordinator
            .compile(req.clone())
            .with_timeout()
            .await
            .unwrap();
        assert!(response.success, "stderr: {}", response.stderr);

        coordinator.shutdown().await?;

        Ok(())
    }

    #[tokio::test]
    #[snafu::report]
    async fn still_usable_after_idle() -> Result<()> {
        let mut coordinator = new_coordinator();

        let req = CompileRequest {
            code: r#"pub fn alpha(a: u8) -> u8 { a + 1 }"#.into(),
        };

        let res = coordinator
            .compile(req.clone())
            .with_timeout()
            .await
            .unwrap();
        assert!(res.success);

        coordinator.idle().await.unwrap();

        let res = coordinator.compile(req).await.unwrap();
        assert!(res.success);

        coordinator.shutdown().await?;

        Ok(())
    }
/*
    #[tokio::test]
    #[snafu::report]
    async fn exit_due_to_signal_is_reported() -> Result<()> {
        let coordinator = new_coordinator();

        let req = ExecuteRequest {
            channel: Channel::Stable,
            mode: Mode::Release,
            edition: Edition::Rust2021,
            crate_type: CrateType::Binary,
            tests: false,
            backtrace: false,
            code: r#"fn main() { std::process::abort(); }"#.into(),
        };

        let res = coordinator.execute(req.clone()).await.unwrap();

        assert!(!res.success);
        assert_contains!(res.exit_detail, "abort");

        coordinator.shutdown().await?;

        Ok(())
    }

    fn new_execution_limited_request() -> ExecuteRequest {
        ExecuteRequest {
            channel: Channel::Stable,
            mode: Mode::Debug,
            edition: Edition::Rust2021,
            crate_type: CrateType::Binary,
            tests: false,
            backtrace: false,
            code: Default::default(),
        }
    }

    #[tokio::test]
    #[snafu::report]
    async fn network_connections_are_disabled() -> Result<()> {
        // The limits are only applied to the container
        let coordinator = new_coordinator_docker();

        let req = ExecuteRequest {
            code: r#"
                fn main() {
                    match ::std::net::TcpStream::connect("google.com:80") {
                        Ok(_) => println!("Able to connect to the outside world"),
                        Err(e) => println!("Failed to connect {}, {:?}", e, e),
                    }
                }
            "#
            .into(),
            ..new_execution_limited_request()
        };

        let res = coordinator.execute(req).with_timeout().await.unwrap();

        assert_contains!(res.stdout, "Failed to connect");

        coordinator.shutdown().await?;

        Ok(())
    }

    #[tokio::test]
    #[snafu::report]
    async fn memory_usage_is_limited() -> Result<()> {
        // The limits are only applied to the container
        let coordinator = new_coordinator_docker();

        let req = ExecuteRequest {
            code: r#"
                fn main() {
                    let gigabyte = 1024 * 1024 * 1024;
                    let mut big = vec![0u8; 1 * gigabyte];
                    for i in &mut big { *i += 1; }
                }
            "#
            .into(),
            ..new_execution_limited_request()
        };

        let res = coordinator.execute(req).with_timeout().await.unwrap();

        assert!(!res.success);
        // TODO: We need to actually inform the user about this somehow. The UI is blank.
        // assert_contains!(res.stdout, "Killed");

        coordinator.shutdown().await?;

        Ok(())
    }

    #[tokio::test]
    #[snafu::report]
    async fn number_of_pids_is_limited() -> Result<()> {
        // The limits are only applied to the container
        let coordinator = new_coordinator_docker();

        let req = ExecuteRequest {
            code: r##"
                fn main() {
                    ::std::process::Command::new("sh").arg("-c").arg(r#"
                        z() {
                            z&
                            z
                        }
                        z
                    "#).status().unwrap();
                }
            "##
            .into(),
            ..new_execution_limited_request()
        };

        let res = coordinator.execute(req).with_timeout().await.unwrap();

        assert_contains!(res.stderr, "Cannot fork");

        coordinator.shutdown().await?;

        Ok(())
    }
*/

    static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
        let millis = env::var("TESTS_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10000);
        Duration::from_millis(millis)
    });

    trait TimeoutExt: Future + Sized {
        async fn with_timeout(self) -> Self::Output {
            tokio::time::timeout(*TIMEOUT, self)
                .await
                .expect("The operation timed out")
        }
    }

    impl<F: Future> TimeoutExt for F {}

}
