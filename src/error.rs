use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmasmError {
    #[error("RPC call failed: {0}")]
    RpcError(String),

    #[error("Bytecode assembly failed: {0}")]
    AssemblyError(String),

    #[error("Failed to decode response: {0}")]
    DecodeError(String),

    #[error("Batch too large: {size} items exceeds maximum {max}")]
    BatchTooLarge { size: usize, max: usize },

    #[error("Provider error: {0}")]
    ProviderError(String),
}

impl From<alloy::transports::TransportError> for EmasmError {
    fn from(e: alloy::transports::TransportError) -> Self {
        EmasmError::RpcError(e.to_string())
    }
}
