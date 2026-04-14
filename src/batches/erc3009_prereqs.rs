//! Batch ERC-3009 prerequisite queries for x402 payment verification
//!
//! Replaces 4 sequential RPC calls with a single batch call:
//! 1. Token name (EIP-712 domain)
//! 2. Token version (EIP-712 domain)
//! 3. Payer balance
//! 4. Authorization nonce state

use crate::{
    bytecode::CallSpec,
    error::EmasmError,
};
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol;
use alloy::sol_types::SolCall;

sol! {
    #[sol(rpc)]
    contract ERC3009Token {
        function name() external view returns (string);
        function version() external view returns (string);
        function balanceOf(address account) external view returns (uint256);
        function authorizationState(address authorizer, bytes32 nonce) external view returns (bool);
    }
}

/// ERC-3009 prerequisites for payment verification
#[derive(Debug, Clone)]
pub struct Erc3009Prereqs {
    /// Token name for EIP-712 domain
    pub name: String,
    /// Token version for EIP-712 domain
    pub version: String,
    /// Payer's token balance
    pub balance: U256,
    /// Whether the nonce has been used
    pub nonce_used: bool,
}

/// Batch query all ERC-3009 prerequisites in a single RPC call
pub async fn batch_erc3009_prereqs<T, P>(
    provider: &P,
    token: Address,
    payer: Address,
    nonce: FixedBytes<32>,
) -> Result<Erc3009Prereqs, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // Build calls
    let name_call = CallSpec {
        target: token,
        calldata: Bytes::from(ERC3009Token::nameCall {}.abi_encode()),
        return_size: 96, // string encoding: offset (32) + length (32) + data (32+)
        use_call: false,
    };

    let version_call = CallSpec {
        target: token,
        calldata: Bytes::from(ERC3009Token::versionCall {}.abi_encode()),
        return_size: 96,
        use_call: false,
    };

    let balance_call = CallSpec {
        target: token,
        calldata: Bytes::from(ERC3009Token::balanceOfCall { account: payer }.abi_encode()),
        return_size: 32,
        use_call: false,
    };

    let auth_call = CallSpec {
        target: token,
        calldata: Bytes::from(
            ERC3009Token::authorizationStateCall {
                authorizer: payer,
                nonce,
            }
            .abi_encode(),
        ),
        return_size: 32,
        use_call: false,
    };

    let calls = vec![name_call, version_call, balance_call, auth_call];
    let response = super::execute_batch(provider, &calls).await?;

    // Decode results
    // Each result is padded to 32-byte boundaries based on return_size
    let name_end = 96;
    let version_end = name_end + 96;
    let balance_end = version_end + 32;
    let auth_end = balance_end + 32;

    if response.len() < auth_end {
        return Err(EmasmError::DecodeError(format!(
            "Response too short: {} < {}",
            response.len(),
            auth_end
        )));
    }

    let name = decode_string(&response[0..name_end])?;
    let version = decode_string(&response[name_end..version_end])?;
    let balance = U256::from_be_slice(&response[version_end..balance_end]);
    let nonce_used = !response[auth_end - 1] == 0; // Last byte of bool

    Ok(Erc3009Prereqs {
        name,
        version,
        balance,
        nonce_used,
    })
}

/// Decode ABI-encoded string from bytes
fn decode_string(data: &[u8]) -> Result<String, EmasmError> {
    if data.len() < 64 {
        return Err(EmasmError::DecodeError("String data too short".into()));
    }

    // First 32 bytes: offset to string data
    let offset = U256::from_be_slice(&data[0..32]);
    let offset = offset.to::<usize>();

    if offset + 32 > data.len() {
        // Fallback: maybe it's inline without offset
        let len = U256::from_be_slice(&data[0..32]).to::<usize>();
        if len < 32 && len + 32 <= data.len() {
            return Ok(String::from_utf8_lossy(&data[32..32 + len]).to_string());
        }
        return Err(EmasmError::DecodeError("Invalid string offset".into()));
    }

    // At offset: 32 bytes length, then string data
    let len = U256::from_be_slice(&data[offset..offset + 32]).to::<usize>();
    if offset + 32 + len > data.len() {
        return Err(EmasmError::DecodeError("String length exceeds data".into()));
    }

    Ok(String::from_utf8_lossy(&data[offset + 32..offset + 32 + len]).to_string())
}

/// Cached token metadata (immutable, can cache forever)
#[derive(Debug, Clone)]
pub struct TokenMetadata {
    pub name: String,
    pub version: String,
}

/// Query just the immutable token metadata (for caching)
pub async fn batch_token_metadata<T, P>(
    provider: &P,
    token: Address,
) -> Result<TokenMetadata, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let name_call = CallSpec {
        target: token,
        calldata: Bytes::from(ERC3009Token::nameCall {}.abi_encode()),
        return_size: 96,
        use_call: false,
    };

    let version_call = CallSpec {
        target: token,
        calldata: Bytes::from(ERC3009Token::versionCall {}.abi_encode()),
        return_size: 96,
        use_call: false,
    };

    let calls = vec![name_call, version_call];
    let response = super::execute_batch(provider, &calls).await?;

    let name_end = 96;
    let version_end = name_end + 96;

    if response.len() < version_end {
        return Err(EmasmError::DecodeError(format!(
            "Response too short: {} < {}",
            response.len(),
            version_end
        )));
    }

    let name = decode_string(&response[0..name_end])?;
    let version = decode_string(&response[name_end..version_end])?;

    Ok(TokenMetadata { name, version })
}
