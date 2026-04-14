//! Batch fee queries for LP Locker
//!
//! Replaces 2 sequential RPC calls (isLocked + getClaimableFees) with a single batch call.

use crate::{
    bytecode::CallSpec,
    error::EmasmError,
    BATCH_CHUNK_SIZE,
};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol;
use alloy::sol_types::SolCall;

// LP Locker V4 function selectors
sol! {
    #[sol(rpc)]
    contract Cat402LpLockerV4 {
        function isLocked(address token) external view returns (bool);
        function getClaimableFees(address token) external view returns (uint256 amount0, uint256 amount1);
    }
}

/// Result of fee query for a single token
#[derive(Debug, Clone)]
pub struct FeeQueryResult {
    pub token: Address,
    pub is_locked: bool,
    pub amount0: U256,
    pub amount1: U256,
    pub success: bool,
}

/// Batch query isLocked + getClaimableFees for multiple tokens
///
/// Returns 2 results per token: is_locked (bool) and fees (amount0, amount1)
/// Reduces 2N RPC calls to 1 call for N tokens.
pub async fn batch_fee_queries<T, P>(
    provider: &P,
    lp_locker: Address,
    tokens: &[Address],
) -> Result<Vec<FeeQueryResult>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    // Chunk if needed
    let chunks = super::chunk_batch(tokens, BATCH_CHUNK_SIZE / 2); // 2 calls per token
    let mut all_results = Vec::with_capacity(tokens.len());

    for chunk in chunks {
        // Build calls: for each token, add isLocked and getClaimableFees
        let mut calls: Vec<CallSpec> = Vec::with_capacity(chunk.len() * 2);

        for token in &chunk {
            // isLocked call
            let is_locked_calldata = Cat402LpLockerV4::isLockedCall { token: *token }.abi_encode();
            calls.push(CallSpec {
                target: lp_locker,
                calldata: Bytes::from(is_locked_calldata),
                return_size: 32, // bool padded to 32 bytes
                use_call: false,
            });

            // getClaimableFees call
            let fees_calldata = Cat402LpLockerV4::getClaimableFeesCall { token: *token }.abi_encode();
            calls.push(CallSpec {
                target: lp_locker,
                calldata: Bytes::from(fees_calldata),
                return_size: 64, // (uint256, uint256)
                use_call: false,
            });
        }

        let response = super::execute_batch(provider, &calls).await?;

        // Decode results: alternating isLocked (32 bytes) and fees (64 bytes)
        let mut offset = 0;
        for token in &chunk {
            // isLocked result (32 bytes)
            let is_locked = if offset + 32 <= response.len() {
                U256::from_be_slice(&response[offset..offset + 32]) != U256::ZERO
            } else {
                false
            };
            offset += 32;

            // getClaimableFees result (64 bytes: amount0, amount1)
            let (amount0, amount1) = if offset + 64 <= response.len() {
                let a0 = U256::from_be_slice(&response[offset..offset + 32]);
                let a1 = U256::from_be_slice(&response[offset + 32..offset + 64]);
                (a0, a1)
            } else {
                (U256::ZERO, U256::ZERO)
            };
            offset += 64;

            all_results.push(FeeQueryResult {
                token: *token,
                is_locked,
                amount0,
                amount1,
                success: true,
            });
        }
    }

    Ok(all_results)
}

/// Query fee info for a single token (convenience wrapper)
pub async fn query_token_fees<T, P>(
    provider: &P,
    lp_locker: Address,
    token: Address,
) -> Result<FeeQueryResult, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let results = batch_fee_queries(provider, lp_locker, &[token]).await?;
    results.into_iter().next().ok_or_else(|| EmasmError::DecodeError("No result".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calldata_encoding() {
        let token = Address::repeat_byte(0x42);
        let calldata = Cat402LpLockerV4::isLockedCall { token }.abi_encode();
        // Should be 4 bytes selector + 32 bytes address
        assert_eq!(calldata.len(), 36);
    }
}
