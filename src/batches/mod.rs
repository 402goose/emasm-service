//! Batch operation implementations for 402.cat platform

mod wallet_balances;
mod erc3009_prereqs;
mod pool_discovery;
mod position_search;
mod fee_query;
mod orchestrator_state;
mod token_metadata;
mod quote_batch;
mod identity_batch;
mod v3_quote_batch;
mod swap_quoter;

pub use wallet_balances::*;
pub use erc3009_prereqs::*;
pub use pool_discovery::*;
pub use position_search::*;
pub use fee_query::*;
pub use orchestrator_state::*;
pub use token_metadata::*;
pub use quote_batch::*;
pub use identity_batch::*;
pub use v3_quote_batch::*;
pub use swap_quoter::*;

use crate::{bytecode::CallSpec, EmasmError};
use alloy::primitives::Bytes;
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;

/// Execute a batch of calls and return raw response bytes
pub async fn execute_batch<T, P>(
    provider: &P,
    calls: &[CallSpec],
) -> Result<Bytes, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if calls.is_empty() {
        return Ok(Bytes::new());
    }

    let bytecode = crate::bytecode::build_batch_bytecode(calls);

    tracing::debug!(
        bytecode_len = bytecode.len(),
        num_calls = calls.len(),
        "Built batch bytecode"
    );

    // Execute via eth_call with no from/to (code execution only)
    let tx = alloy::rpc::types::TransactionRequest::default()
        .input(bytecode.clone().into());

    let result = provider
        .call(&tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Batch eth_call failed");
            EmasmError::RpcError(e.to_string())
        })?;

    tracing::debug!(
        result_len = result.len(),
        "Batch eth_call succeeded"
    );

    Ok(result)
}

/// Chunk a batch if it exceeds the maximum size
pub fn chunk_batch<T: Clone>(items: &[T], chunk_size: usize) -> Vec<Vec<T>> {
    items
        .chunks(chunk_size)
        .map(|chunk| chunk.to_vec())
        .collect()
}
