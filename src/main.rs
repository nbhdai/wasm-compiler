// wasm-compiler/src/main.rs

use futures_util::StreamExt;
use std::{
    future::{self, Future},
    net::{IpAddr, Ipv6Addr, SocketAddr},
};
use tarpc::{
    server::{self, incoming::Incoming, Channel},
    tokio_serde::formats::Bincode,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use service_compiler::{service::WasmCompilerService, WasmCompiler};

async fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            "service_compiler=debug,tarpc=info,orchestrator=trace",
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Get configuration from environment
    let port = std::env::var("WASM_COMPILER_PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()?;

    let addr: SocketAddr = "0.0.0.0:8080".parse()?;

    // Create compiler server
    let service = WasmCompilerService::new().await?;
    let mut listener = tarpc::serde_transport::tcp::listen(&addr, Bincode::default).await?;
    tracing::info!("Listening on {}", listener.local_addr());
    listener.config_mut().max_frame_length(usize::MAX);
    listener
        // Ignore accept errors.
        .filter_map(|r| match r {
            Ok(r) => {
                tracing::debug!("Connection");
                future::ready(Some(r))
            }
            Err(err) => {
                tracing::error!(?err, "No Connection");
                future::ready(None)
            }
        })
        .map(server::BaseChannel::with_defaults)
        // Limit channels to 1 per IP.
        //.max_channels_per_key(5, |t| t.transport().peer_addr().unwrap().ip())
        .map(|channel| {
            tracing::debug!(ip=?channel.transport().peer_addr().unwrap().ip(), "Serving");
            channel.execute(service.clone().serve())
        })
        .for_each(|service| async {
            service
                .map(spawn)
                .buffer_unordered(5)
                .for_each(|_| async {})
                .await
        })
        .await;
    Ok(())
}
