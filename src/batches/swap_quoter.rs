//! Unified Swap Quoter with Batched RPC Calls
//!
//! Batches V3 + V4 quote lookups and token metadata into minimal RPC calls.
//!
//! # RPC Call Reduction
//!
//! | Operation | Before | After |
//! |-----------|--------|-------|
//! | Token metadata | 6 calls | 1 call |
//! | V3 quotes (3 fee tiers) | 3 calls | 1 call |
//! | V4 quotes (pool checks + quotes) | 10-40 calls | 2 calls |
//! | **Total** | ~20-50 calls | ~4 calls |

use super::{
    batch_erc20_metadata, batch_v3_quotes, batch_v4_quotes,
    Erc20Metadata, PoolKey, QuoteRequest as V4QuoteRequest,
};
use crate::error::EmasmError;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;

/// WETH addresses by chain
pub fn weth_address(chain_id: u64) -> Address {
    match chain_id {
        8453 | 84532 => "0x4200000000000000000000000000000000000006".parse().unwrap(),
        _ => "0x4200000000000000000000000000000000000006".parse().unwrap(),
    }
}

/// V3 Quoter V2 addresses by chain
pub fn v3_quoter_address(chain_id: u64) -> Option<Address> {
    match chain_id {
        8453 => Some("0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a".parse().unwrap()),
        84532 => Some("0xC5290058841028F1614F3A6F0F5816cAd0df5E27".parse().unwrap()),
        _ => None,
    }
}

/// V4 Quoter addresses by chain
pub fn v4_quoter_address(chain_id: u64) -> Option<Address> {
    match chain_id {
        8453 => Some("0x0d5e0f971ed27fbff6c2837bf31316121532048d".parse().unwrap()),
        84532 => Some("0x4a6513c898fe1b2d0e78d3b0e0a4a151589b1cba".parse().unwrap()),
        _ => None,
    }
}

/// Universal Router addresses by chain
pub fn universal_router_address(chain_id: u64) -> Option<Address> {
    match chain_id {
        8453 => Some("0x6fF5693b99212Da76ad316178A184AB56D299b43".parse().unwrap()),
        84532 => Some("0x492E6456D9528771018DeB9E87ef7750EF184104".parse().unwrap()),
        _ => None,
    }
}

/// Special ETH address
pub const ETH_ADDRESS: &str = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE";
pub const ZERO_ADDRESS: Address = Address::ZERO;

/// Check if address represents native ETH
pub fn is_eth_address(addr: &str) -> bool {
    let normalized = addr.to_lowercase();
    normalized == ETH_ADDRESS.to_lowercase() ||
    normalized == "0x0000000000000000000000000000000000000000" ||
    normalized == "eth"
}

/// Input for swap quote
#[derive(Debug, Clone)]
pub struct SwapQuoteInput {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub chain_id: u64,
    /// Optional V4 hook addresses to try
    pub v4_hooks: Option<Vec<Address>>,
}

/// Quote from a single source
#[derive(Debug, Clone)]
pub struct SwapQuote {
    pub provider: String,
    pub amount_out: U256,
    pub fee_bps: Option<u32>,
    pub gas_estimate: Option<U256>,
    pub route_description: String,
}

/// Output from batched swap quoter
#[derive(Debug, Clone)]
pub struct SwapQuoteOutput {
    /// Token metadata for input token
    pub token_in_metadata: Option<Erc20Metadata>,
    /// Token metadata for output token
    pub token_out_metadata: Option<Erc20Metadata>,
    /// Best quote across all sources
    pub best_quote: Option<SwapQuote>,
    /// All quotes received (for debugging/comparison)
    pub all_quotes: Vec<SwapQuote>,
    /// Number of RPC calls made
    pub rpc_calls_made: usize,
}

/// Known V4 hook addresses on Base
///
/// Add your own hook addresses here for custom pool discovery.
fn known_v4_hooks() -> Vec<Address> {
    vec![
        Address::ZERO, // Vanilla V4 (no hooks)
    ]
}

/// V4 fee tiers and tick spacings
const V4_FEE_TIERS: [(u32, i32); 4] = [
    (100, 1),    // 0.01%
    (500, 10),   // 0.05%
    (3000, 60),  // 0.3%
    (10000, 200), // 1%
];

/// Get batched swap quotes using EMASM
///
/// This is the main entry point that replaces the TypeScript quoter.
/// It batches all RPC calls to minimize latency and cost.
pub async fn get_batched_swap_quotes<T, P>(
    provider: &P,
    input: SwapQuoteInput,
) -> Result<SwapQuoteOutput, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let mut rpc_calls = 0;
    let mut all_quotes = Vec::new();

    // 1. Batch token metadata (1 RPC call for both tokens)
    let tokens_to_query: Vec<Address> = vec![input.token_in, input.token_out]
        .into_iter()
        .filter(|addr| *addr != Address::ZERO) // Don't query metadata for native ETH
        .collect();

    let metadata_results = if !tokens_to_query.is_empty() {
        rpc_calls += 1;
        batch_erc20_metadata::<T, P>(provider, &tokens_to_query).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let token_in_metadata = metadata_results.iter().find(|m| m.address == input.token_in).cloned();
    let token_out_metadata = metadata_results.iter().find(|m| m.address == input.token_out).cloned();

    let token_in_symbol = token_in_metadata.as_ref()
        .map(|m| m.symbol.clone())
        .unwrap_or_else(|| if input.token_in == Address::ZERO { "ETH".to_string() } else { "???".to_string() });
    let token_out_symbol = token_out_metadata.as_ref()
        .map(|m| m.symbol.clone())
        .unwrap_or_else(|| if input.token_out == Address::ZERO { "ETH".to_string() } else { "???".to_string() });

    // 2. Get V3 quotes (1 RPC call for all 3 fee tiers)
    if let Some(quoter) = v3_quoter_address(input.chain_id) {
        rpc_calls += 1;
        let v3_results = batch_v3_quotes::<T, P>(
            provider,
            quoter,
            input.token_in,
            input.token_out,
            input.amount_in,
        ).await.unwrap_or_default();

        for result in v3_results {
            if result.success {
                all_quotes.push(SwapQuote {
                    provider: "Uniswap V3".to_string(),
                    amount_out: result.amount_out,
                    fee_bps: Some(result.fee),
                    gas_estimate: Some(result.gas_estimate),
                    route_description: format!(
                        "{} -[V3 {}%]-> {}",
                        token_in_symbol,
                        result.fee as f64 / 10000.0,
                        token_out_symbol
                    ),
                });
            }
        }
    }

    // 3. Get V4 quotes (1-2 RPC calls for all pool combinations)
    if let Some(quoter) = v4_quoter_address(input.chain_id) {
        // Combine custom hooks with known hooks
        let mut hooks_to_try: Vec<Address> = input.v4_hooks.unwrap_or_default();
        hooks_to_try.extend(known_v4_hooks());
        // Dedupe
        hooks_to_try.sort();
        hooks_to_try.dedup();

        // Sort tokens for V4 pool key (currency0 < currency1)
        let (currency0, currency1, zero_for_one) = if input.token_in < input.token_out {
            (input.token_in, input.token_out, true)
        } else {
            (input.token_out, input.token_in, false)
        };

        // Build all V4 quote requests
        let mut v4_requests: Vec<V4QuoteRequest> = Vec::new();
        for &(fee, tick_spacing) in &V4_FEE_TIERS {
            for hooks in &hooks_to_try {
                v4_requests.push(V4QuoteRequest {
                    pool_key: PoolKey::new(
                        currency0,
                        currency1,
                        fee,
                        tick_spacing,
                        *hooks,
                    ),
                    zero_for_one,
                    amount_in: input.amount_in.try_into().unwrap_or(u128::MAX),
                });
            }
        }

        if !v4_requests.is_empty() {
            rpc_calls += 1;
            let v4_results = batch_v4_quotes::<T, P>(provider, quoter, &v4_requests)
                .await
                .unwrap_or_default();

            for (i, result) in v4_results.iter().enumerate() {
                if result.success && result.amount_out > 0 {
                    let req = &v4_requests[i];
                    let hook_name = if req.pool_key.hooks == Address::ZERO {
                        "vanilla".to_string()
                    } else {
                        format!("hook:{}", &format!("{:?}", req.pool_key.hooks)[..10])
                    };

                    all_quotes.push(SwapQuote {
                        provider: "Uniswap V4".to_string(),
                        amount_out: U256::from(result.amount_out),
                        fee_bps: Some(req.pool_key.fee),
                        gas_estimate: Some(U256::from(result.gas_estimate)),
                        route_description: format!(
                            "{} -[V4 {}% {}]-> {}",
                            token_in_symbol,
                            req.pool_key.fee as f64 / 10000.0,
                            hook_name,
                            token_out_symbol
                        ),
                    });
                }
            }
        }
    }

    // Find best quote (highest output)
    let best_quote = all_quotes.iter()
        .max_by_key(|q| q.amount_out)
        .cloned();

    tracing::info!(
        token_in = %input.token_in,
        token_out = %input.token_out,
        amount_in = %input.amount_in,
        quotes_received = all_quotes.len(),
        rpc_calls = rpc_calls,
        best_provider = best_quote.as_ref().map(|q| q.provider.as_str()).unwrap_or("none"),
        best_amount_out = %best_quote.as_ref().map(|q| q.amount_out).unwrap_or(U256::ZERO),
        "Batched swap quotes complete"
    );

    Ok(SwapQuoteOutput {
        token_in_metadata,
        token_out_metadata,
        best_quote,
        all_quotes,
        rpc_calls_made: rpc_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_eth_address() {
        assert!(is_eth_address("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"));
        assert!(is_eth_address("0x0000000000000000000000000000000000000000"));
        assert!(is_eth_address("eth"));
        assert!(!is_eth_address("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")); // USDC
    }

    #[test]
    fn test_chain_addresses() {
        // Base mainnet
        assert!(v3_quoter_address(8453).is_some());
        assert!(v4_quoter_address(8453).is_some());
        assert!(universal_router_address(8453).is_some());

        // Base Sepolia
        assert!(v3_quoter_address(84532).is_some());
        assert!(v4_quoter_address(84532).is_some());
        assert!(universal_router_address(84532).is_some());
    }
}
