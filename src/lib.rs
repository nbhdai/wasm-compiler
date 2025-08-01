pub mod client;
pub mod service;

use std::net::SocketAddr;

use anyhow::Context;
use futures::future;
use thiserror::Error;

pub use client::CompilerClient;
pub use service::WasmCompilerService;

#[derive(Debug, Error, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompilerError {
    #[error("Missing Code")]
    MissingCode,
    #[error("Io Error: {0}")]
    IoError(String),
    #[error("Compilation Error: {0}")]
    CompilationError(String),
    #[error("Read Error: {0}")]
    ReadError(String),
}

// Implement conversions from other error types
impl From<std::io::Error> for CompilerError {
    fn from(error: std::io::Error) -> Self {
        CompilerError::IoError(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompileResponse {
    /// This should be a signature.
    pub sha256: String,
    pub stdout: String,
    pub stderr: String,
    pub wasm_binary: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompileRequest {
    pub code: String,
}

#[tarpc::service]
pub trait WasmCompiler {
    /// Compile Rust code to WASM binary
    async fn compile(request: CompileRequest) -> Result<CompileResponse, CompilerError>;
}

pub async fn connect_env() -> anyhow::Result<Vec<CompilerClient>> {
    let compiler_addr_str =
        std::env::var("COMPILER").with_context(|| "COMPILER environment variable should be set")?;
    
    let connection_futures = compiler_addr_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(connect)
        .collect::<Vec<_>>();
    future::join_all(connection_futures).await.into_iter().collect()
}


pub async fn connect(compiler_addr_str: &str) -> anyhow::Result<CompilerClient> {

    let mut candidate_addrs = Vec::new();

    // First, try parsing as a direct SocketAddr
    match compiler_addr_str.parse::<SocketAddr>() {
        Ok(addr) => {
            candidate_addrs.push(addr);
            tracing::debug!(
                "'{}' parsed directly as a SocketAddr: {}",
                compiler_addr_str,
                addr
            );
        }
        Err(_) => {
            // If not a direct SocketAddr, assume it's a hostname and perform DNS resolution
            tracing::debug!(
                "'{}' is not a direct SocketAddr, attempting DNS resolution...",
                compiler_addr_str
            );
            let resolved_addrs: Vec<SocketAddr> =
                tokio::net::lookup_host(&compiler_addr_str).await?.collect();

            if resolved_addrs.is_empty() {
                anyhow::bail!(
                    "DNS resolution for '{}' returned no addresses",
                    compiler_addr_str
                );
            }

            tracing::debug!(
                "Resolved '{}' to the following addresses: {:?}",
                compiler_addr_str,
                resolved_addrs
            );
            candidate_addrs.extend(resolved_addrs);
        }
    };

    let mut last_error = None;

    // Iterate through all candidate addresses and try to connect
    for addr in candidate_addrs {
        tracing::debug!("Attempting to connect to compiler at {}...", addr);
        match CompilerClient::connect(addr).await {
            Ok(client) => {
                tracing::debug!("Successfully connected to compiler at {}", addr);
                return Ok(client); // Return the client on the first successful connection
            }
            Err(e) => {
                tracing::error!("Failed to connect to {}: {:?}", addr, e);
                last_error = Some(e); // Keep track of the last error
            }
        }
    }

    // If we've exhausted all addresses and haven't connected, return an error
    let error_msg = format!(
        "Failed to connect to compiler using any of the addresses derived from '{}'. Last error: {:?}",
        compiler_addr_str,
        last_error
    );
    Err(anyhow::Error::msg(error_msg))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn test_compile_grader_basic() {
        tracing::debug!("Starting test_compile_grader_basic");
        dotenv::dotenv().unwrap();
        let clients = connect_env().await.unwrap();
        let client = &clients[0];

        let grader_code = r#"
fn call(output: &str, target: &str) -> Result<f32, String> {
    if output.trim() == target.trim() {
        Ok(100.0)
    } else {
        Ok(0.0)
    }
}
        "#;

        let result = client.compile(grader_code).await.unwrap();
        println!("{}", result.stdout);
        println!("===========================");
        println!("{}", result.stderr);
        println!("===========================");
        let binary = result.wasm_binary;
        assert!(!binary.is_empty(), "Binary should not be empty");
        tracing::trace!("Grader binary size: {} bytes", binary.len());

        tracing::debug!("Starting test_compile_generator_basic");

        let generator_code = r#"
fn call(model_input: &str) -> Result<String, String> {
    Ok(format!("Generated response for: {}", model_input))
}
        "#;

        let result = client.compile(generator_code).await.unwrap();
        println!("{}", result.stdout);
        println!("===========================");
        println!("{}", result.stderr);
        println!("===========================");
        let binary = result.wasm_binary;

        assert!(!binary.is_empty(), "Binary should not be empty");
        tracing::trace!("Generator binary size: {} bytes", binary.len());
    }
}
