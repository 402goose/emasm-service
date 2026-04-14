//! Batch V3 quote queries
//!
//! Replaces sequential quoteExactInputSingle calls with a single batch call.
//! Queries all fee tiers (500, 3000, 10000) in one RPC call.
//!
//! Reduces 3 RPC calls to 1.

use crate::{
    bytecode::CallSpec,
    error::EmasmError,
};
use alloy::primitives::{Address, Bytes, U256, U160};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol;
use alloy::sol_types::SolCall;

// V3 Quoter V2 function
sol! {
    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    #[sol(rpc)]
    contract QuoterV2 {
        function quoteExactInputSingle(QuoteExactInputSingleParams calldata params)
            external
            returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
    }
}

/// V3 fee tiers (in basis points)
pub const V3_FEE_TIERS: [u32; 3] = [500, 3000, 10000];

/// Single V3 quote request
#[derive(Debug, Clone)]
pub struct V3QuoteRequest {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub fee: u32,
}

/// V3 quote result
#[derive(Debug, Clone)]
pub struct V3QuoteResult {
    pub fee: u32,
    pub amount_out: U256,
    pub sqrt_price_after: U256,
    pub gas_estimate: U256,
    pub success: bool,
}

/// Batch execute V3 quotes for all fee tiers
///
/// Queries 500, 3000, and 10000 bps pools in a single eth_call.
/// Returns best quote among successful pools.
pub async fn batch_v3_quotes<T, P>(
    provider: &P,
    quoter_v2: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Vec<V3QuoteResult>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let requests: Vec<V3QuoteRequest> = V3_FEE_TIERS
        .iter()
        .map(|&fee| V3QuoteRequest {
            token_in,
            token_out,
            amount_in,
            fee,
        })
        .collect();

    batch_v3_quotes_multi(provider, quoter_v2, &requests).await
}

/// Batch execute multiple V3 quote requests
///
/// Each request can have different tokens/amounts/fees.
/// All quotes are executed in a single eth_call.
pub async fn batch_v3_quotes_multi<T, P>(
    provider: &P,
    quoter_v2: Address,
    requests: &[V3QuoteRequest],
) -> Result<Vec<V3QuoteResult>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if requests.is_empty() {
        return Ok(Vec::new());
    }

    let chunks = super::chunk_batch(requests, crate::BATCH_CHUNK_SIZE);
    let mut all_results = Vec::with_capacity(requests.len());

    for chunk in chunks {
        let calls: Vec<CallSpec> = chunk
            .iter()
            .map(|req| {
                let params = QuoteExactInputSingleParams {
                    tokenIn: req.token_in,
                    tokenOut: req.token_out,
                    amountIn: req.amount_in,
                    fee: alloy::primitives::Uint::<24, 1>::from(req.fee),
                    sqrtPriceLimitX96: U160::ZERO,
                };
                let calldata = QuoterV2::quoteExactInputSingleCall { params }.abi_encode();

                tracing::debug!(
                    fee = req.fee,
                    token_in = %req.token_in,
                    token_out = %req.token_out,
                    "Encoding V3 quote request"
                );

                CallSpec {
                    target: quoter_v2,
                    calldata: Bytes::from(calldata),
                    return_size: 128, // 4 x uint256 (amountOut, sqrtPrice, ticksCrossed, gasEstimate)
                    use_call: true, // QuoterV2 uses internal state simulation, needs CALL
                }
            })
            .collect();

        tracing::debug!(
            num_calls = calls.len(),
            quoter = %quoter_v2,
            "Executing batch V3 quote"
        );

        // Execute batch - failures return zeros, not errors
        let response = match super::execute_batch(provider, &calls).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Batch V3 quote failed");
                // Return all failed results
                return Ok(chunk.iter().map(|req| V3QuoteResult {
                    fee: req.fee,
                    amount_out: U256::ZERO,
                    sqrt_price_after: U256::ZERO,
                    gas_estimate: U256::ZERO,
                    success: false,
                }).collect());
            }
        };

        tracing::debug!(
            response_len = response.len(),
            "Batch V3 quote response received"
        );

        // Parse results (128 bytes per quote)
        for (i, req) in chunk.iter().enumerate() {
            let offset = i * 128;

            if offset + 128 <= response.len() {
                let amount_out = U256::from_be_slice(&response[offset..offset + 32]);
                let sqrt_price_after = U256::from_be_slice(&response[offset + 32..offset + 64]);
                // Skip ticks crossed (offset + 64..offset + 96)
                let gas_estimate = U256::from_be_slice(&response[offset + 96..offset + 128]);

                let success = !amount_out.is_zero();

                tracing::debug!(
                    fee = req.fee,
                    amount_out = %amount_out,
                    success = success,
                    "Parsed V3 quote result"
                );

                all_results.push(V3QuoteResult {
                    fee: req.fee,
                    amount_out,
                    sqrt_price_after,
                    gas_estimate,
                    success,
                });
            } else {
                tracing::warn!(
                    request_idx = i,
                    offset = offset,
                    response_len = response.len(),
                    "Response too short for V3 quote result"
                );
                all_results.push(V3QuoteResult {
                    fee: req.fee,
                    amount_out: U256::ZERO,
                    sqrt_price_after: U256::ZERO,
                    gas_estimate: U256::ZERO,
                    success: false,
                });
            }
        }
    }

    Ok(all_results)
}

/// Find the best V3 quote (highest output) from a list of results
pub fn find_best_v3_quote(results: &[V3QuoteResult]) -> Option<&V3QuoteResult> {
    results
        .iter()
        .filter(|r| r.success)
        .max_by_key(|r| r.amount_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_tiers() {
        assert_eq!(V3_FEE_TIERS.len(), 3);
        assert_eq!(V3_FEE_TIERS[0], 500);   // 0.05%
        assert_eq!(V3_FEE_TIERS[1], 3000);  // 0.3%
        assert_eq!(V3_FEE_TIERS[2], 10000); // 1%
    }
}
