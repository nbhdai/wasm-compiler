// wasm-compiler/src/client.rs

use crate::{CompileRequest, WasmCompilerClient, WasmCompilerRequest, WasmCompilerResponse};
use crate::{CompileResponse, CompilerError};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tarpc::tokio_serde::formats::Bincode;
use tarpc::{client, context, ClientMessage, Response, Transport};

#[derive(Clone)]
pub struct CompilerClient {
    client: Arc<WasmCompilerClient>,
}

impl CompilerClient {
    pub async fn connect(addr: SocketAddr) -> anyhow::Result<Self> {
        let mut sender = tarpc::serde_transport::tcp::connect(&addr, Bincode::default);
        sender.config_mut().max_frame_length(usize::MAX);
        let sender = sender.await?;
        let client = WasmCompilerClient::new(client::Config::default(), sender).spawn();

        Ok(Self {
            client: Arc::new(client),
        })
    }

    pub fn raw_connect<
        T: Transport<ClientMessage<WasmCompilerRequest>, Response<WasmCompilerResponse>>
            + Send
            + 'static,
    >(
        listener: T,
    ) -> Self {
        let client = WasmCompilerClient::new(client::Config::default(), listener).spawn();

        Self {
            client: Arc::new(client),
        }
    }

    pub async fn compile(&self, code: &str) -> Result<CompileResponse, CompilerError> {
        let request = CompileRequest {
            code: code.to_string(),
        };

        let mut context = context::current();
        context.deadline = Instant::now() + Duration::from_secs(120);

        let response = self
            .client
            .compile(context, request)
            .await
            .map_err(|error| {
                tracing::error!(?error, "RPC error");
                CompilerError::IoError(error.to_string())
            })??;

        Ok(response)
    }
}
