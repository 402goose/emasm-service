//! Batch V4 quote queries
//!
//! Replaces sequential quote_v4_swap calls with a single batch call.
//! Critical for multi-hop swaps that need 2+ quotes.

use crate::{
    bytecode::CallSpec,
    error::EmasmError,
};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol_types::SolValue;

/// V4 Pool key parameters
#[derive(Debug, Clone)]
pub struct PoolKey {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
}

/// Single quote request
#[derive(Debug, Clone)]
pub struct QuoteRequest {
    pub pool_key: PoolKey,
    pub zero_for_one: bool,
    pub amount_in: u128,
}

/// Quote result
#[derive(Debug, Clone)]
pub struct QuoteResult {
    pub amount_out: u128,
    pub gas_estimate: u128,
    pub success: bool,
}

/// Batch execute multiple V4 quotes in a single call
///
/// Reduces N quote calls to 1 call.
pub async fn batch_v4_quotes<T, P>(
    provider: &P,
    quoter: Address,
    requests: &[QuoteRequest],
) -> Result<Vec<QuoteResult>, EmasmError>
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
                let calldata = encode_quote_exact_input_single(req);
                tracing::debug!(
                    calldata_len = calldata.len(),
                    calldata_hex = %hex::encode(&calldata[..calldata.len().min(64)]),
                    target = %req.pool_key.currency0,
                    "Generated quote calldata"
                );
                CallSpec {
                    target: quoter,
                    calldata: Bytes::from(calldata),
                    return_size: 64, // (uint256 amountOut, uint256 gasEstimate)
                    use_call: true, // V4 Quoter does internal state simulation, needs CALL not STATICCALL
                }
            })
            .collect();

        tracing::debug!(
            num_calls = calls.len(),
            quoter = %quoter,
            "Executing batch quote"
        );

        let response = super::execute_batch(provider, &calls).await?;

        tracing::debug!(
            response_len = response.len(),
            response_hex = %hex::encode(&response[..response.len().min(256)]),
            num_requests = chunk.len(),
            "Batch quote response received"
        );

        for (i, req) in chunk.iter().enumerate() {
            let offset = i * 64;
            let (amount_out, gas_estimate, success) = if offset + 64 <= response.len() {
                let amount = U256::from_be_slice(&response[offset..offset + 32]);
                let gas = U256::from_be_slice(&response[offset + 32..offset + 64]);

                tracing::debug!(
                    request_idx = i,
                    offset = offset,
                    amount_raw = %amount,
                    gas_raw = %gas,
                    currency0 = %req.pool_key.currency0,
                    currency1 = %req.pool_key.currency1,
                    zero_for_one = req.zero_for_one,
                    amount_in = req.amount_in,
                    "Parsing quote result"
                );

                (
                    amount.try_into().unwrap_or(0u128),
                    gas.try_into().unwrap_or(0u128),
                    amount > U256::ZERO,
                )
            } else {
                tracing::warn!(
                    request_idx = i,
                    offset = offset,
                    response_len = response.len(),
                    "Response too short for quote result"
                );
                (0, 0, false)
            };

            all_results.push(QuoteResult {
                amount_out,
                gas_estimate,
                success,
            });
        }
    }

    Ok(all_results)
}

/// Execute a multi-hop quote (e.g., USDC → CAT → Token) in a single batch
pub async fn batch_multihop_quote<T, P>(
    provider: &P,
    quoter: Address,
    first_hop: QuoteRequest,
    second_hop_pool: PoolKey,
    second_hop_zero_for_one: bool,
) -> Result<(QuoteResult, QuoteResult), EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // First, get the first hop quote
    let first_results = batch_v4_quotes(provider, quoter, &[first_hop.clone()]).await?;
    let first_result = first_results.into_iter().next()
        .ok_or_else(|| EmasmError::DecodeError("No first hop result".to_string()))?;

    if !first_result.success || first_result.amount_out == 0 {
        return Ok((first_result, QuoteResult { amount_out: 0, gas_estimate: 0, success: false }));
    }

    // Use the first hop output as input for second hop
    let second_hop = QuoteRequest {
        pool_key: second_hop_pool,
        zero_for_one: second_hop_zero_for_one,
        amount_in: first_result.amount_out,
    };

    let second_results = batch_v4_quotes(provider, quoter, &[second_hop]).await?;
    let second_result = second_results.into_iter().next()
        .ok_or_else(|| EmasmError::DecodeError("No second hop result".to_string()))?;

    Ok((first_result, second_result))
}

/// Encode quoteExactInputSingle call
///
/// Function: quoteExactInputSingle(QuoteExactSingleParams params)
/// struct QuoteExactSingleParams {
///     PoolKey poolKey;     // (address,address,uint24,int24,address)
///     bool zeroForOne;
///     uint128 exactAmount;
///     bytes hookData;
/// }
fn encode_quote_exact_input_single(req: &QuoteRequest) -> Vec<u8> {
    // Function selector
    let selector = alloy::primitives::keccak256(
        b"quoteExactInputSingle(((address,address,uint24,int24,address),bool,uint128,bytes))"
    );

    // Encode the pool key as a tuple
    let pool_key_tuple = (
        req.pool_key.currency0,
        req.pool_key.currency1,
        alloy::primitives::Uint::<24, 1>::from(req.pool_key.fee),
        alloy::primitives::Signed::<24, 1>::try_from(req.pool_key.tick_spacing).unwrap_or_default(),
        req.pool_key.hooks,
    );

    // Pack QuoteExactSingleParams
    let params_struct = (
        pool_key_tuple,
        req.zero_for_one,
        req.amount_in as u128,
        alloy::primitives::Bytes::new(), // hookData
    );

    // Wrap in outer tuple for encoding
    let params = (params_struct,);

    let encoded = params.abi_encode_params();

    let mut calldata = Vec::with_capacity(4 + encoded.len());
    calldata.extend_from_slice(&selector[..4]);
    calldata.extend_from_slice(&encoded);

    calldata
}

/// Helper to create a PoolKey from individual components
impl PoolKey {
    pub fn new(
        currency0: Address,
        currency1: Address,
        fee: u32,
        tick_spacing: i32,
        hooks: Address,
    ) -> Self {
        Self {
            currency0,
            currency1,
            fee,
            tick_spacing,
            hooks,
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_key_creation() {
        let key = PoolKey::new(
            Address::ZERO,
            Address::repeat_byte(1),
            3000,
            60,
            Address::repeat_byte(2),
        );
        assert_eq!(key.fee, 3000);
        assert_eq!(key.tick_spacing, 60);
    }

    #[test]
    fn test_encode_quote() {
        let req = QuoteRequest {
            pool_key: PoolKey::new(
                Address::ZERO,
                Address::repeat_byte(1),
                3000,
                60,
                Address::repeat_byte(2),
            ),
            zero_for_one: true,
            amount_in: 1_000_000,
        };

        let calldata = encode_quote_exact_input_single(&req);
        // Should have selector (4) + encoded params
        assert!(calldata.len() > 4);
    }
}
