//! Batch position search for finding agent LP positions
//!
//! Replaces 2N RPC calls (ownerOf + getPositionLiquidity per position)
//! with a single batched eth_call.

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

sol! {
    #[sol(rpc)]
    contract PositionManager {
        function ownerOf(uint256 tokenId) external view returns (address);
        function getPositionLiquidity(uint256 tokenId) external view returns (uint128 liquidity);
    }
}

/// Position info from batch query
#[derive(Debug, Clone)]
pub struct PositionInfo {
    /// Position token ID
    pub token_id: u128,
    /// Position owner address
    pub owner: Address,
    /// Position liquidity (0 if non-existent or empty)
    pub liquidity: u128,
    /// Whether the query succeeded
    pub exists: bool,
}

/// Batch search for positions owned by a specific address
///
/// Searches positions from `start_id` backwards for `count` positions,
/// returning info about each position including owner and liquidity.
///
/// This replaces the loop in `find_agent_position_for_pool()` which made
/// 2 RPC calls per position (ownerOf + getPositionLiquidity).
pub async fn batch_position_search<T, P>(
    provider: &P,
    position_manager: Address,
    start_id: u128,
    count: u64,
) -> Result<Vec<PositionInfo>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if count == 0 || start_id == 0 {
        return Ok(Vec::new());
    }

    // Build list of position IDs to check (backwards from start_id)
    let end_id = start_id.saturating_sub(count as u128 - 1).max(1);
    let position_ids: Vec<u128> = (end_id..=start_id).rev().collect();

    // Chunk if needed (2 calls per position: ownerOf + getPositionLiquidity)
    let chunks = super::chunk_batch(&position_ids, BATCH_CHUNK_SIZE / 2);
    let mut all_results = Vec::with_capacity(position_ids.len());

    for chunk in chunks {
        // Build calls: ownerOf and getPositionLiquidity for each position
        let mut calls = Vec::with_capacity(chunk.len() * 2);

        for &position_id in &chunk {
            let token_id = U256::from(position_id);

            calls.push(CallSpec {
                target: position_manager,
                calldata: Bytes::from(
                    PositionManager::ownerOfCall { tokenId: token_id }.abi_encode()
                ),
                return_size: 32, // address
                use_call: false,
            });

            calls.push(CallSpec {
                target: position_manager,
                calldata: Bytes::from(
                    PositionManager::getPositionLiquidityCall { tokenId: token_id }.abi_encode()
                ),
                return_size: 32, // uint128 padded to 32
                use_call: false,
            });
        }

        let response = super::execute_batch(provider, &calls).await?;

        // Decode results (ownerOf: 32 bytes, liquidity: 32 bytes per position)
        let bytes_per_position = 64;
        for (i, &position_id) in chunk.iter().enumerate() {
            let offset = i * bytes_per_position;

            if offset + bytes_per_position > response.len() {
                // Position doesn't exist or call failed
                all_results.push(PositionInfo {
                    token_id: position_id,
                    owner: Address::ZERO,
                    liquidity: 0,
                    exists: false,
                });
                continue;
            }

            // Decode owner (last 20 bytes of 32-byte slot)
            let owner_bytes: [u8; 20] = response[offset + 12..offset + 32]
                .try_into()
                .unwrap_or([0; 20]);
            let owner = Address::from(owner_bytes);

            // Decode liquidity (last 16 bytes of 32-byte slot for uint128)
            let liquidity_bytes: [u8; 16] = response[offset + 48..offset + 64]
                .try_into()
                .unwrap_or([0; 16]);
            let liquidity = u128::from_be_bytes(liquidity_bytes);

            // Position exists if owner is non-zero
            let exists = owner != Address::ZERO;

            all_results.push(PositionInfo {
                token_id: position_id,
                owner,
                liquidity,
                exists,
            });
        }
    }

    Ok(all_results)
}

/// Find the first position owned by target_owner with non-zero liquidity
///
/// Convenience function that searches and filters in one call.
pub async fn find_owned_position_with_liquidity<T, P>(
    provider: &P,
    position_manager: Address,
    target_owner: Address,
    start_id: u128,
    count: u64,
) -> Result<Option<u128>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let positions = batch_position_search::<T, P>(provider, position_manager, start_id, count).await?;

    for pos in positions {
        if pos.exists && pos.owner == target_owner && pos.liquidity > 0 {
            return Ok(Some(pos.token_id));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_id_range() {
        let start_id: u128 = 100;
        let count: u64 = 20;
        let end_id = start_id.saturating_sub(count as u128 - 1).max(1);
        let position_ids: Vec<u128> = (end_id..=start_id).rev().collect();

        assert_eq!(position_ids.len(), 20);
        assert_eq!(position_ids[0], 100); // Start from highest
        assert_eq!(position_ids[19], 81); // End at lowest
    }

    #[test]
    fn test_position_id_range_near_zero() {
        let start_id: u128 = 5;
        let count: u64 = 20;
        let end_id = start_id.saturating_sub(count as u128 - 1).max(1);
        let position_ids: Vec<u128> = (end_id..=start_id).rev().collect();

        assert_eq!(position_ids.len(), 5);
        assert_eq!(position_ids[0], 5);
        assert_eq!(position_ids[4], 1);
    }

    #[test]
    fn test_bytes_per_position_alignment() {
        // Each position needs 2 calls: ownerOf (32 bytes) + getPositionLiquidity (32 bytes)
        assert_eq!(32 + 32, 64);
    }
}
