//! Batch pool discovery for DEX routing
//!
//! Efficiently query multiple pool states for quote comparison.
//! Includes on-chain pool discovery without DB dependency.

use crate::{
    bytecode::CallSpec,
    error::EmasmError,
    BATCH_CHUNK_SIZE,
};
use alloy::primitives::{Address, Bytes, U256, keccak256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

sol! {
    #[sol(rpc)]
    contract StateView {
        function getSlot0(bytes32 poolId) external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint24 protocolFee,
            uint24 lpFee
        );

        function getLiquidity(bytes32 poolId) external view returns (uint128);
    }

    #[sol(rpc)]
    contract ERC20Supply {
        function totalSupply() external view returns (uint256);
    }
}

/// Pool state from V4 StateView
#[derive(Debug, Clone)]
pub struct PoolState {
    pub pool_id: [u8; 32],
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub protocol_fee: u32,
    pub lp_fee: u32,
    pub liquidity: u128,
}

/// Batch query V4 pool states
pub async fn batch_pool_states<T, P>(
    provider: &P,
    state_view: Address,
    pool_ids: &[[u8; 32]],
) -> Result<Vec<PoolState>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if pool_ids.is_empty() {
        return Ok(Vec::new());
    }

    let chunks = super::chunk_batch(pool_ids, BATCH_CHUNK_SIZE / 2); // 2 calls per pool
    let mut all_results = Vec::with_capacity(pool_ids.len());

    for chunk in chunks {
        // Build calls: getSlot0 and getLiquidity for each pool
        let mut calls = Vec::with_capacity(chunk.len() * 2);

        for pool_id in &chunk {
            let pool_id_bytes = alloy::primitives::FixedBytes::from_slice(pool_id);

            calls.push(CallSpec {
                target: state_view,
                calldata: Bytes::from(StateView::getSlot0Call { poolId: pool_id_bytes }.abi_encode()),
                return_size: 128, // 4 return values
                use_call: false,
            });

            calls.push(CallSpec {
                target: state_view,
                calldata: Bytes::from(StateView::getLiquidityCall { poolId: pool_id_bytes }.abi_encode()),
                return_size: 32,
                use_call: false,
            });
        }

        let response = super::execute_batch(provider, &calls).await?;

        // Decode results (slot0: 128 bytes, liquidity: 32 bytes per pool)
        let bytes_per_pool = 128 + 32;
        for (i, pool_id) in chunk.iter().enumerate() {
            let offset = i * bytes_per_pool;

            if offset + bytes_per_pool > response.len() {
                // Partial results - pool may not exist
                all_results.push(PoolState {
                    pool_id: *pool_id,
                    sqrt_price_x96: U256::ZERO,
                    tick: 0,
                    protocol_fee: 0,
                    lp_fee: 0,
                    liquidity: 0,
                });
                continue;
            }

            // Decode slot0 (sqrtPriceX96: uint160, tick: int24, protocolFee: uint24, lpFee: uint24)
            let sqrt_price_x96 = U256::from_be_slice(&response[offset..offset + 32]);
            // Tick is int24 but encoded as int256, take last 4 bytes as i32
            let tick_bytes: [u8; 4] = response[offset + 60..offset + 64].try_into().unwrap_or([0; 4]);
            let tick = i32::from_be_bytes(tick_bytes);
            let protocol_fee = u32::from_be_bytes(
                response[offset + 92..offset + 96].try_into().unwrap_or([0; 4])
            );
            let lp_fee = u32::from_be_bytes(
                response[offset + 124..offset + 128].try_into().unwrap_or([0; 4])
            );

            // Decode liquidity
            let liquidity_offset = offset + 128;
            let liquidity = u128::from_be_bytes(
                response[liquidity_offset + 16..liquidity_offset + 32]
                    .try_into()
                    .unwrap_or([0; 16])
            );

            all_results.push(PoolState {
                pool_id: *pool_id,
                sqrt_price_x96,
                tick,
                protocol_fee,
                lp_fee,
                liquidity,
            });
        }
    }

    Ok(all_results)
}

/// Token info for routing
#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub address: Address,
    pub total_supply: U256,
}

/// Batch query token total supplies (for liquidity estimation)
pub async fn batch_token_supplies<T, P>(
    provider: &P,
    tokens: &[Address],
) -> Result<Vec<TokenInfo>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let chunks = super::chunk_batch(tokens, BATCH_CHUNK_SIZE);
    let mut all_results = Vec::with_capacity(tokens.len());

    for chunk in chunks {
        let calls: Vec<CallSpec> = chunk
            .iter()
            .map(|token| CallSpec {
                target: *token,
                calldata: Bytes::from(ERC20Supply::totalSupplyCall {}.abi_encode()),
                return_size: 32,
                use_call: false,
            })
            .collect();

        let response = super::execute_batch(provider, &calls).await?;

        for (i, token) in chunk.iter().enumerate() {
            let offset = i * 32;
            let supply = if offset + 32 <= response.len() {
                U256::from_be_slice(&response[offset..offset + 32])
            } else {
                U256::ZERO
            };

            all_results.push(TokenInfo {
                address: *token,
                total_supply: supply,
            });
        }
    }

    Ok(all_results)
}

// ============================================================================
// V4 Pool Discovery - On-Chain Pool Detection Without DB
// ============================================================================

/// Default pool configuration constants
pub mod pool_defaults {
    /// Dynamic fee flag
    pub const DYNAMIC_FEE_FLAG: u32 = 0x800000;
    /// Default tick spacing
    pub const TICK_SPACING: i32 = 60;
}

/// Parameters for discovering a V4 pool
#[derive(Debug, Clone)]
pub struct PoolDiscoveryParams {
    /// The token address (will be paired with CAT or USDC)
    pub token: Address,
    /// The paired currency (CAT for graduated tokens, USDC for CAT itself)
    pub paired_currency: Address,
    /// The hook address
    pub hook: Address,
}

/// Result of pool discovery
#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    /// The pool key components
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
    /// Computed pool ID
    pub pool_id: [u8; 32],
    /// Pool state (if pool exists)
    pub state: Option<PoolState>,
    /// Whether the pool exists (has liquidity)
    pub exists: bool,
}

impl DiscoveredPool {
    /// Check if this is the token we're looking for (vs the paired currency)
    pub fn is_token(&self, token: Address) -> bool {
        self.currency0 == token || self.currency1 == token
    }

    /// Get the paired currency (the one that isn't the token)
    pub fn get_paired_currency(&self, token: Address) -> Address {
        if self.currency0 == token {
            self.currency1
        } else {
            self.currency0
        }
    }

    /// Determine if token is currency0 (for zero_for_one direction)
    pub fn is_zero_for_one(&self, token_in: Address) -> bool {
        self.currency0 == token_in
    }
}

/// Compute the V4 pool ID from pool key components
///
/// Pool ID = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks))
pub fn compute_pool_id(
    currency0: Address,
    currency1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
) -> [u8; 32] {
    // Encode as (address, address, uint24, int24, address)
    // Note: Solidity encodes uint24/int24 as full 32-byte words
    let encoded = (
        currency0,
        currency1,
        alloy::primitives::Uint::<24, 1>::from(fee),
        alloy::primitives::Signed::<24, 1>::try_from(tick_spacing).unwrap_or_default(),
        hooks,
    ).abi_encode();

    keccak256(&encoded).into()
}

/// Discover a V4 pool for a single token
///
/// Computes the expected pool key and checks if the pool exists on-chain
/// via StateView.
pub async fn discover_pool<T, P>(
    provider: &P,
    state_view: Address,
    params: PoolDiscoveryParams,
) -> Result<DiscoveredPool, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // Sort currencies (V4 requires currency0 < currency1)
    let (currency0, currency1) = if params.token < params.paired_currency {
        (params.token, params.paired_currency)
    } else {
        (params.paired_currency, params.token)
    };

    let fee = pool_defaults::DYNAMIC_FEE_FLAG;
    let tick_spacing = pool_defaults::TICK_SPACING;

    // Compute pool ID
    let pool_id = compute_pool_id(currency0, currency1, fee, tick_spacing, params.hook);

    // Query pool state
    let states = batch_pool_states::<T, P>(provider, state_view, &[pool_id]).await?;
    let state = states.into_iter().next();

    // Pool exists if it has non-zero sqrtPriceX96 (initialized)
    let exists = state.as_ref()
        .map(|s| s.sqrt_price_x96 > U256::ZERO)
        .unwrap_or(false);

    Ok(DiscoveredPool {
        currency0,
        currency1,
        fee,
        tick_spacing,
        hooks: params.hook,
        pool_id,
        state,
        exists,
    })
}

/// Batch discover V4 pools for multiple tokens
///
/// Efficiently discovers pools for multiple tokens in a single batch call.
/// All tokens are assumed to use the same paired currency and hook.
pub async fn batch_discover_pools<T, P>(
    provider: &P,
    state_view: Address,
    tokens: &[Address],
    paired_currency: Address,
    hook: Address,
) -> Result<Vec<DiscoveredPool>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let fee = pool_defaults::DYNAMIC_FEE_FLAG;
    let tick_spacing = pool_defaults::TICK_SPACING;

    // Compute all pool IDs and track the pool key components
    let pool_data: Vec<_> = tokens.iter().map(|token| {
        let (currency0, currency1) = if *token < paired_currency {
            (*token, paired_currency)
        } else {
            (paired_currency, *token)
        };
        let pool_id = compute_pool_id(currency0, currency1, fee, tick_spacing, hook);
        (currency0, currency1, pool_id)
    }).collect();

    // Extract just the pool IDs for batch query
    let pool_ids: Vec<[u8; 32]> = pool_data.iter().map(|(_, _, id)| *id).collect();

    // Batch query all pool states
    let states = batch_pool_states::<T, P>(provider, state_view, &pool_ids).await?;

    // Combine results
    let results = pool_data.iter().zip(states.into_iter()).map(|((c0, c1, pool_id), state)| {
        let exists = state.sqrt_price_x96 > U256::ZERO;
        DiscoveredPool {
            currency0: *c0,
            currency1: *c1,
            fee,
            tick_spacing,
            hooks: hook,
            pool_id: *pool_id,
            state: Some(state),
            exists,
        }
    }).collect();

    Ok(results)
}

/// Discover both possible pools for a token against two quote currencies
///
/// Returns up to 2 discovered pools, one for each quote currency pairing.
/// Useful for determining the correct routing for a token.
pub async fn discover_token_pools<T, P>(
    provider: &P,
    state_view: Address,
    token: Address,
    cat_address: Address,
    usdc_address: Address,
    hook: Address,
) -> Result<(Option<DiscoveredPool>, Option<DiscoveredPool>), EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // Compute pool IDs for both possible pairings
    let fee = pool_defaults::DYNAMIC_FEE_FLAG;
    let tick_spacing = pool_defaults::TICK_SPACING;

    let (cat_c0, cat_c1) = if token < cat_address {
        (token, cat_address)
    } else {
        (cat_address, token)
    };
    let cat_pool_id = compute_pool_id(cat_c0, cat_c1, fee, tick_spacing, hook);

    let (usdc_c0, usdc_c1) = if token < usdc_address {
        (token, usdc_address)
    } else {
        (usdc_address, token)
    };
    let usdc_pool_id = compute_pool_id(usdc_c0, usdc_c1, fee, tick_spacing, hook);

    // Batch query both pools
    let states = batch_pool_states::<T, P>(provider, state_view, &[cat_pool_id, usdc_pool_id]).await?;

    let cat_state = states.get(0).cloned();
    let usdc_state = states.get(1).cloned();

    let cat_pool = cat_state.map(|state| {
        let exists = state.sqrt_price_x96 > U256::ZERO;
        DiscoveredPool {
            currency0: cat_c0,
            currency1: cat_c1,
            fee,
            tick_spacing,
            hooks: hook,
            pool_id: cat_pool_id,
            state: Some(state),
            exists,
        }
    }).filter(|p| p.exists);

    let usdc_pool = usdc_state.map(|state| {
        let exists = state.sqrt_price_x96 > U256::ZERO;
        DiscoveredPool {
            currency0: usdc_c0,
            currency1: usdc_c1,
            fee,
            tick_spacing,
            hooks: hook,
            pool_id: usdc_pool_id,
            state: Some(state),
            exists,
        }
    }).filter(|p| p.exists);

    Ok((cat_pool, usdc_pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_pool_id() {
        // Test with known addresses
        let token = Address::repeat_byte(0x01);
        let cat = Address::repeat_byte(0x02);
        let hook = Address::repeat_byte(0x08);

        let pool_id = compute_pool_id(
            token,
            cat,
            pool_defaults::DYNAMIC_FEE_FLAG,
            pool_defaults::TICK_SPACING,
            hook,
        );

        // Pool ID should be 32 bytes
        assert_eq!(pool_id.len(), 32);
        // Should be deterministic
        let pool_id2 = compute_pool_id(
            token,
            cat,
            pool_defaults::DYNAMIC_FEE_FLAG,
            pool_defaults::TICK_SPACING,
            hook,
        );
        assert_eq!(pool_id, pool_id2);
    }

    #[test]
    fn test_currency_sorting() {
        let token_low = Address::repeat_byte(0x01);
        let token_high = Address::repeat_byte(0x02);

        // Pool ID should be the same regardless of input order
        let id1 = compute_pool_id(
            token_low, token_high,
            pool_defaults::DYNAMIC_FEE_FLAG,
            pool_defaults::TICK_SPACING,
            Address::ZERO,
        );
        let id2 = compute_pool_id(
            token_high, token_low, // Reversed!
            pool_defaults::DYNAMIC_FEE_FLAG,
            pool_defaults::TICK_SPACING,
            Address::ZERO,
        );

        // These should be DIFFERENT because we're not sorting in compute_pool_id
        // The caller (discover functions) handles sorting
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_known_pool_id_computation() {
        use std::str::FromStr;

        // Known pool data verified on-chain
        // Verifies pool ID computation matches V4
        let currency0 = Address::from_str("0x0049e7082ed3715671d8f55c574f929622c70402").unwrap();
        let currency1 = Address::from_str("0x402a813310f92630848c93a65746110cdb2b0402").unwrap();
        let hooks = Address::from_str("0x08fa6267515a60fe40779366646eb54e87d0c0c0").unwrap();
        let fee = 8388608u32; // 0x800000 = DYNAMIC_FEE_FLAG
        let tick_spacing = 60i32;

        let computed_id = compute_pool_id(currency0, currency1, fee, tick_spacing, hooks);

        // Expected pool ID from database (verified working on-chain)
        let expected_hex = "15cfd0c2f337093c5688515cfaa91128934503c70a2805afefd40cdf91bdcaeb";
        let expected_bytes: [u8; 32] = hex::decode(expected_hex)
            .expect("valid hex")
            .try_into()
            .expect("32 bytes");

        assert_eq!(
            computed_id,
            expected_bytes,
            "Pool ID mismatch!\nComputed: {}\nExpected: {}",
            hex::encode(computed_id),
            expected_hex
        );
    }
}
