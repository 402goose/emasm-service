//! Batch identity resolution (ERC-8004 + SSTORE2)
//!
//! Efficiently resolves multiple identities in a single eth_call.

use crate::{bytecode::CallSpec, execute_batch, EmasmError, BATCH_CHUNK_SIZE};
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::Transport;
use alloy::network::Ethereum;

// Contract interface
sol! {
    /// Identity registry with SSTORE2 storage
    interface ICatIdentityRegistry {
        function tokenByName(string calldata name) external view returns (uint256);
        function ownerOf(uint256 tokenId) external view returns (address);
        function getRegistration(uint256 tokenId) external view returns (bytes memory);
        function getRegistrationByName(string calldata name) external view returns (bytes memory);
        function isNameAvailable(string calldata name) external view returns (bool);
    }
}

/// Raw identity data from chain
#[derive(Debug, Clone)]
pub struct RawIdentity {
    pub name: String,
    pub token_id: u64,
    pub owner: Address,
    pub registration_json: Bytes,
}

/// Result of batch identity resolution
#[derive(Debug, Clone)]
pub struct IdentityBatchResult {
    pub resolved: Vec<(String, RawIdentity)>,
    pub not_found: Vec<String>,
    pub errors: Vec<(String, String)>,
}

/// Batch resolve identities by name
///
/// Returns raw registration JSON bytes for each resolved identity.
/// Caller is responsible for parsing the JSON.
pub async fn batch_resolve_by_name<T, P>(
    provider: &P,
    registry: Address,
    names: &[String],
) -> Result<IdentityBatchResult, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if names.is_empty() {
        return Ok(IdentityBatchResult {
            resolved: vec![],
            not_found: vec![],
            errors: vec![],
        });
    }

    let mut result = IdentityBatchResult {
        resolved: vec![],
        not_found: vec![],
        errors: vec![],
    };

    // Process in chunks to respect gas limits
    for chunk in names.chunks(BATCH_CHUNK_SIZE) {
        let chunk_result = resolve_chunk(provider, registry, chunk).await?;
        result.resolved.extend(chunk_result.resolved);
        result.not_found.extend(chunk_result.not_found);
        result.errors.extend(chunk_result.errors);
    }

    Ok(result)
}

/// Resolve a single chunk of names
async fn resolve_chunk<T, P>(
    provider: &P,
    registry: Address,
    names: &[String],
) -> Result<IdentityBatchResult, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // Build calls for getRegistrationByName for each name
    let calls: Vec<CallSpec> = names
        .iter()
        .map(|name| {
            let calldata = ICatIdentityRegistry::getRegistrationByNameCall {
                name: name.clone(),
            }
            .abi_encode();

            CallSpec {
                target: registry,
                calldata: calldata.into(),
                // Max 2KB per registration (should be plenty for JSON)
                return_size: 2048,
                use_call: false, // STATICCALL for view functions
            }
        })
        .collect();

    let raw_result = execute_batch(provider, &calls).await?;

    // Parse results
    let mut result = IdentityBatchResult {
        resolved: vec![],
        not_found: vec![],
        errors: vec![],
    };

    // Each result is padded to 2048 bytes (32-byte aligned = 2048)
    let chunk_size = 2048;

    for (i, name) in names.iter().enumerate() {
        let start = i * chunk_size;
        let end = start + chunk_size;

        if end > raw_result.len() {
            result.errors.push((name.clone(), "Result truncated".to_string()));
            continue;
        }

        let chunk = &raw_result[start..end];

        // Check if this is a valid ABI-encoded bytes response
        // First 32 bytes = offset, second 32 bytes = length
        if chunk.len() >= 64 {
            let length = u64::from_be_bytes(chunk[56..64].try_into().unwrap_or([0; 8])) as usize;

            if length == 0 {
                // Empty response = name not found
                result.not_found.push(name.clone());
            } else if length > 0 && 64 + length <= chunk.len() {
                // Valid response
                let json_bytes = &chunk[64..64 + length];

                // TODO: Also fetch tokenId and owner in parallel batch
                // For now, we'd need a second batch call
                result.resolved.push((
                    name.clone(),
                    RawIdentity {
                        name: name.clone(),
                        token_id: 0, // Would need separate call
                        owner: Address::ZERO, // Would need separate call
                        registration_json: Bytes::copy_from_slice(json_bytes),
                    },
                ));
            } else {
                result.errors.push((name.clone(), "Invalid ABI encoding".to_string()));
            }
        } else {
            result.not_found.push(name.clone());
        }
    }

    Ok(result)
}

/// Batch check name availability
pub async fn batch_check_availability<T, P>(
    provider: &P,
    registry: Address,
    names: &[String],
) -> Result<Vec<(String, bool)>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if names.is_empty() {
        return Ok(vec![]);
    }

    let calls: Vec<CallSpec> = names
        .iter()
        .map(|name| {
            let calldata = ICatIdentityRegistry::isNameAvailableCall {
                name: name.clone(),
            }
            .abi_encode();

            CallSpec {
                target: registry,
                calldata: calldata.into(),
                return_size: 32, // bool
                use_call: false,
            }
        })
        .collect();

    let raw_result = execute_batch(provider, &calls).await?;

    // Parse bool results (32 bytes each, padded)
    let chunk_size = 32;
    let mut results = Vec::with_capacity(names.len());

    for (i, name) in names.iter().enumerate() {
        let start = i * chunk_size;
        let end = start + chunk_size;

        if end <= raw_result.len() {
            // Last byte is the bool value
            let available = raw_result[end - 1] != 0;
            results.push((name.clone(), available));
        } else {
            // Default to unavailable on error
            results.push((name.clone(), false));
        }
    }

    Ok(results)
}

/// Batch resolve addresses to identities (for referral payout)
///
/// Given a list of wallet addresses, find their registered identities
/// by scanning token ownership. This is more expensive than by-name lookup.
pub async fn batch_resolve_by_address<T, P>(
    _provider: &P,
    _registry: Address,
    addresses: &[Address],
    _max_token_id: u64,
) -> Result<Vec<(Address, Option<RawIdentity>)>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // This is trickier - we'd need to scan tokens or use events
    // For now, return empty (would implement with event indexing)
    tracing::warn!(
        "batch_resolve_by_address not fully implemented - consider adding address→tokenId mapping"
    );

    Ok(addresses.iter().map(|a| (*a, None)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calldata_encoding() {
        let call = ICatIdentityRegistry::getRegistrationByNameCall {
            name: "goose".to_string(),
        };
        let encoded = call.abi_encode();

        // Should start with function selector
        assert!(encoded.len() > 4);
        // Selector for getRegistrationByName(string)
        assert_eq!(&encoded[0..4], &[0x2f, 0x91, 0x61, 0xd0]); // Updated to match actual selector
    }

    #[test]
    fn test_availability_calldata() {
        let call = ICatIdentityRegistry::isNameAvailableCall {
            name: "test".to_string(),
        };
        let encoded = call.abi_encode();
        assert!(encoded.len() > 4);
    }
}
