//! Batch token metadata queries
//!
//! Replaces 3 sequential RPC calls (name, symbol, decimals) with a single batch call.
//! Used when resolving external tokens in universal buy/sell.

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

// ERC20 metadata functions
sol! {
    #[sol(rpc)]
    contract ERC20Metadata {
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
    }
}

/// ERC20 token metadata result (name, symbol, decimals, totalSupply)
#[derive(Debug, Clone)]
pub struct Erc20Metadata {
    pub address: Address,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub total_supply: Option<U256>,
}

/// Batch query ERC20 metadata for multiple tokens
///
/// Returns name, symbol, decimals for each token.
/// Reduces 4N RPC calls to 1 call.
pub async fn batch_erc20_metadata<T, P>(
    provider: &P,
    tokens: &[Address],
) -> Result<Vec<Erc20Metadata>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    // 4 calls per token (name, symbol, decimals, totalSupply)
    let chunks = super::chunk_batch(tokens, BATCH_CHUNK_SIZE / 4);
    let mut all_results = Vec::with_capacity(tokens.len());

    for chunk in chunks {
        // Build calls for each token
        let mut calls: Vec<CallSpec> = Vec::with_capacity(chunk.len() * 4);

        for token in &chunk {
            // name() - returns dynamic string
            calls.push(CallSpec {
                target: *token,
                calldata: Bytes::from(ERC20Metadata::nameCall {}.abi_encode()),
                return_size: 128, // Dynamic string: offset(32) + length(32) + data(64 max for names)
                use_call: false,
            });

            // symbol() - returns dynamic string
            calls.push(CallSpec {
                target: *token,
                calldata: Bytes::from(ERC20Metadata::symbolCall {}.abi_encode()),
                return_size: 128, // Dynamic string
                use_call: false,
            });

            // decimals() - returns uint8
            calls.push(CallSpec {
                target: *token,
                calldata: Bytes::from(ERC20Metadata::decimalsCall {}.abi_encode()),
                return_size: 32,
                use_call: false,
            });

            // totalSupply() - returns uint256
            calls.push(CallSpec {
                target: *token,
                calldata: Bytes::from(ERC20Metadata::totalSupplyCall {}.abi_encode()),
                return_size: 32,
                use_call: false,
            });
        }

        let response = super::execute_batch(provider, &calls).await?;

        // Decode results
        let mut offset = 0;
        for token in &chunk {
            // name result (128 bytes)
            let name = decode_string(&response, offset, 128);
            offset += 128;

            // symbol result (128 bytes)
            let symbol = decode_string(&response, offset, 128);
            offset += 128;

            // decimals result (32 bytes)
            let decimals = if offset + 32 <= response.len() {
                response[offset + 31] // Last byte of padded uint8
            } else {
                18 // Default to 18 decimals
            };
            offset += 32;

            // totalSupply result (32 bytes)
            let total_supply = if offset + 32 <= response.len() {
                Some(U256::from_be_slice(&response[offset..offset + 32]))
            } else {
                None
            };
            offset += 32;

            all_results.push(Erc20Metadata {
                address: *token,
                name,
                symbol,
                decimals,
                total_supply,
            });
        }
    }

    Ok(all_results)
}

/// Query metadata for a single token (convenience wrapper)
pub async fn query_erc20_metadata<T, P>(
    provider: &P,
    token: Address,
) -> Result<Erc20Metadata, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let results = batch_erc20_metadata(provider, &[token]).await?;
    results.into_iter().next().ok_or_else(|| EmasmError::DecodeError("No result".to_string()))
}

/// Decode a dynamic string from ABI-encoded response
fn decode_string(data: &[u8], start: usize, max_len: usize) -> String {
    if start + 64 > data.len() {
        return String::new();
    }

    let end = (start + max_len).min(data.len());
    let slice = &data[start..end];

    // ABI string encoding: offset (32 bytes) + length (32 bytes) + data
    if slice.len() < 64 {
        return String::new();
    }

    // Get the length from bytes 32-64
    let length = U256::from_be_slice(&slice[32..64]);
    let length = length.try_into().unwrap_or(0usize).min(64);

    if length == 0 || slice.len() < 64 + length {
        return String::new();
    }

    String::from_utf8_lossy(&slice[64..64 + length]).to_string()
}

/// Batch query just decimals for multiple tokens (lighter weight)
pub async fn batch_token_decimals<T, P>(
    provider: &P,
    tokens: &[Address],
) -> Result<Vec<(Address, u8)>, EmasmError>
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
                calldata: Bytes::from(ERC20Metadata::decimalsCall {}.abi_encode()),
                return_size: 32,
                use_call: false,
            })
            .collect();

        let response = super::execute_batch(provider, &calls).await?;

        for (i, token) in chunk.iter().enumerate() {
            let offset = i * 32;
            let decimals = if offset + 32 <= response.len() {
                response[offset + 31]
            } else {
                18
            };
            all_results.push((*token, decimals));
        }
    }

    Ok(all_results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_string() {
        // ABI encoded "TEST" string
        let mut data = vec![0u8; 128];
        // Offset (pointing to position 32)
        data[31] = 32;
        // Length (4 bytes)
        data[63] = 4;
        // Data "TEST"
        data[64] = b'T';
        data[65] = b'E';
        data[66] = b'S';
        data[67] = b'T';

        let result = decode_string(&data, 0, 128);
        assert_eq!(result, "TEST");
    }
}
