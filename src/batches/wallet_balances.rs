//! Batch wallet balance queries
//!
//! Replaces 30+ individual RPC calls (10 wallets × 3 tokens) with a single batch call.

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

// ERC20 balanceOf selector
sol! {
    #[sol(rpc)]
    contract ERC20Balance {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Result of a single balance query
#[derive(Debug, Clone)]
pub struct BalanceResult {
    pub wallet: Address,
    pub token: Address,
    pub balance: U256,
    pub success: bool,
}

/// Batch query wallet balances for multiple wallets and tokens
///
/// Returns results in order: for each wallet, all token balances in token order.
/// Example: wallets [w1, w2], tokens [t1, t2] → [w1-t1, w1-t2, w2-t1, w2-t2]
pub async fn batch_wallet_balances<T, P>(
    provider: &P,
    wallets: &[Address],
    tokens: &[Address],
) -> Result<Vec<BalanceResult>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if wallets.is_empty() || tokens.is_empty() {
        return Ok(Vec::new());
    }

    // Build all wallet-token pairs
    let pairs: Vec<(Address, Address)> = wallets
        .iter()
        .flat_map(|w| tokens.iter().map(move |t| (*w, *t)))
        .collect();

    // Chunk if needed
    let chunks = super::chunk_batch(&pairs, BATCH_CHUNK_SIZE);
    let mut all_results = Vec::with_capacity(pairs.len());

    for chunk in chunks {
        let calls: Vec<CallSpec> = chunk
            .iter()
            .map(|(wallet, token)| {
                let calldata = ERC20Balance::balanceOfCall { account: *wallet }.abi_encode();
                CallSpec {
                    target: *token,
                    calldata: Bytes::from(calldata),
                    return_size: 32, // uint256
                    use_call: false,
                }
            })
            .collect();

        let response = super::execute_batch(provider, &calls).await?;

        // Decode results (32 bytes per balance, padded to 32-byte alignment)
        for (i, (wallet, token)) in chunk.iter().enumerate() {
            let offset = i * 32;
            let balance = if offset + 32 <= response.len() {
                U256::from_be_slice(&response[offset..offset + 32])
            } else {
                U256::ZERO
            };

            // Non-zero balance or valid zero is success; failure returns zero too
            // but we can't distinguish without checking call success in bytecode
            all_results.push(BalanceResult {
                wallet: *wallet,
                token: *token,
                balance,
                success: true, // Assume success; failed calls return 0
            });
        }
    }

    Ok(all_results)
}

/// Query native ETH balances for multiple wallets
pub async fn batch_eth_balances<T, P>(
    provider: &P,
    wallets: &[Address],
) -> Result<Vec<(Address, U256)>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    // ETH balances require BALANCE opcode, not STATICCALL
    // For now, we'll use parallel futures (can optimize later with custom bytecode)
    use futures::future::try_join_all;

    let futures: Vec<_> = wallets
        .iter()
        .map(|wallet| async move {
            let balance = provider
                .get_balance(*wallet)
                .await
                .map_err(|e| EmasmError::RpcError(e.to_string()))?;
            Ok::<_, EmasmError>((*wallet, balance))
        })
        .collect();

    try_join_all(futures).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pair_generation_order() {
        let wallets = vec![Address::ZERO, Address::repeat_byte(1)];
        let tokens = vec![Address::repeat_byte(2), Address::repeat_byte(3)];

        let pairs: Vec<(Address, Address)> = wallets
            .iter()
            .flat_map(|w| tokens.iter().map(move |t| (*w, *t)))
            .collect();

        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0], (Address::ZERO, Address::repeat_byte(2)));
        assert_eq!(pairs[1], (Address::ZERO, Address::repeat_byte(3)));
        assert_eq!(pairs[2], (Address::repeat_byte(1), Address::repeat_byte(2)));
        assert_eq!(pairs[3], (Address::repeat_byte(1), Address::repeat_byte(3)));
    }
}
