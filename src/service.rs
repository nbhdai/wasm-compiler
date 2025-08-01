use std::sync::Arc;

use crate::{CompileRequest, CompileResponse, CompilerError, WasmCompiler};
use moka::future::Cache; // Import Moka's async cache
use orchestrator::coordinator::limits::{Acquisition, Lifecycle};
use tarpc::context;
use uuid::Uuid;

use orchestrator::coordinator::{limits, Coordinator, CoordinatorFactory, DockerBackend};

// The `CompilationState` enum is no longer needed.

#[derive(Debug, Clone)]
pub struct TraceLifecycle;

impl Lifecycle for TraceLifecycle {
    fn container_start(&self) {
        tracing::trace!("container_start");
    }

    fn container_acquired(&self, how: Acquisition) {
        tracing::trace!("container_acquired {how:?}");
    }

    fn container_release(&self) {
        tracing::trace!("container_release");
    }

    fn process_start(&self) {
        tracing::trace!("process_start");
    }

    fn process_acquired(&self, how: Acquisition) {
        tracing::trace!("process_acquired {how:?}");
    }

    fn process_release(&self) {
        tracing::trace!("process_release");
    }
}

#[derive(Clone)]
pub struct WasmCompilerService {
    pub id: Uuid,
    factory: Arc<CoordinatorFactory>,
    // The previous RwLock<HashMap> and Mutex are replaced by a single Moka cache.
    // The key is the code's SHA256 (String), and the value is the CompileResponse.
    // Moka handles concurrent access and async computation internally.
    cache: Cache<String, CompileResponse>,
}

impl WasmCompiler for WasmCompilerService {
    #[tracing::instrument(skip_all)]
    async fn compile(
        self,
        _ctx: context::Context,
        request: CompileRequest,
    ) -> Result<CompileResponse, CompilerError> {
        println!("====COMPILING====\n {}", request.code);
        // Generate a unique SHA for this compilation request.
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&request.code);
        let code_sha = format!("{:x}", hasher.finalize());
        tracing::info!(code_sha, "Compiling");

        // Use `try_get_with` to get the item from the cache or compute it if it doesn't exist.
        // This is an atomic operation that prevents cache stampedes.
        // If another request for the same `code_sha` is already computing, this call will
        // asynchronously wait for that computation to complete.
        let compile_result = self
            .cache
            .try_get_with(
                code_sha,
                // The async block below is executed only if the item is not in the cache.
                async {
                    let docker_req = orchestrator::coordinator::CompileRequest {
                        code: request.code.clone(),
                    };
                    tracing::info!("Cache miss - starting new compilation");
                    let coordinator: Coordinator<DockerBackend> = self.factory.build();
                    let docker_response = coordinator.compile(docker_req).await;

                    match docker_response {
                        Ok(response) => {
                            let wasm_binary: Vec<u8> = response.response.code.into();
                            let mut hasher = Sha256::new();
                            hasher.update(&wasm_binary);
                            let sha256 = format!("{:x}", hasher.finalize());
                            tracing::info!("Compilation successful, caching result");
                            Ok(CompileResponse {
                                sha256,
                                stdout: response.stdout,
                                stderr: response.stderr,
                                wasm_binary,
                            })
                        }
                        Err(error) => {
                            tracing::error!("Compilation failed: {}", error);
                            // Returning an Err here prevents the result from being cached.
                            Err(CompilerError::CompilationError(format!(
                                "Orchestrator error: {:?}",
                                error
                            )))
                        }
                    }
                },
            )
            .await;

        // `try_get_with` returns a Result<V, Arc<E>>, where V is the value and E is the error.
        // We need to convert the Arc<CompilerError> back to CompilerError.
        // Note: This requires `CompilerError` to implement `Clone`.
        match compile_result {
            Ok(response) => {
                tracing::info!("Cache hit - returning result");
                Ok(response)
            }
            Err(arc_error) => Err((*arc_error).clone()),
        }
    }
}

impl WasmCompilerService {
    pub async fn new() -> anyhow::Result<Self> {
        let limits = Arc::new(limits::Global::with_lifecycle(5, 5, TraceLifecycle));
        let factory = Arc::new(CoordinatorFactory::new(limits));

        let cache = Cache::new(10_000);

        Ok(Self {
            id: Uuid::new_v4(),
            factory,
            cache,
        })
    }
}