//! Batch orchestrator state queries
//!
//! Replaces 4+ sequential RPC calls to GraduationOrchestratorV4 with a single batch call.
//! Queries: cat(), platformTokenActive(), isGraduated(), authorizedAgents()

use crate::{
    bytecode::CallSpec,
    error::EmasmError,
};
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol;
use alloy::sol_types::SolCall;

// Orchestrator function selectors
sol! {
    #[sol(rpc)]
    contract GraduationOrchestratorV4 {
        function cat() external view returns (address);
        function platformTokenActive() external view returns (bool);
        function isGraduated(address token) external view returns (bool);
        function authorizedAgents(address agent) external view returns (bool);
        function hook() external view returns (address);
        function usdc() external view returns (address);
        function lpLocker() external view returns (address);
    }
}

/// Full orchestrator state result
#[derive(Debug, Clone)]
pub struct OrchestratorState {
    pub cat_address: Address,
    pub platform_token_active: bool,
    pub hook_address: Address,
    pub usdc_address: Address,
    pub lp_locker_address: Address,
}

/// Query core orchestrator state in a single batch call
///
/// Reduces 5 RPC calls to 1.
pub async fn batch_orchestrator_state<T, P>(
    provider: &P,
    orchestrator: Address,
) -> Result<OrchestratorState, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    let calls = vec![
        CallSpec {
            target: orchestrator,
            calldata: Bytes::from(GraduationOrchestratorV4::catCall {}.abi_encode()),
            return_size: 32, // address
            use_call: false,
        },
        CallSpec {
            target: orchestrator,
            calldata: Bytes::from(GraduationOrchestratorV4::platformTokenActiveCall {}.abi_encode()),
            return_size: 32, // bool
            use_call: false,
        },
        CallSpec {
            target: orchestrator,
            calldata: Bytes::from(GraduationOrchestratorV4::hookCall {}.abi_encode()),
            return_size: 32, // address
            use_call: false,
        },
        CallSpec {
            target: orchestrator,
            calldata: Bytes::from(GraduationOrchestratorV4::usdcCall {}.abi_encode()),
            return_size: 32, // address
            use_call: false,
        },
        CallSpec {
            target: orchestrator,
            calldata: Bytes::from(GraduationOrchestratorV4::lpLockerCall {}.abi_encode()),
            return_size: 32, // address
            use_call: false,
        },
    ];

    let response = super::execute_batch(provider, &calls).await?;

    // Decode 5 x 32-byte results
    if response.len() < 160 {
        return Err(EmasmError::DecodeError(format!(
            "Orchestrator state response too short: {} bytes",
            response.len()
        )));
    }

    let cat_address = Address::from_slice(&response[12..32]);
    let platform_token_active = U256::from_be_slice(&response[32..64]) != U256::ZERO;
    let hook_address = Address::from_slice(&response[76..96]);
    let usdc_address = Address::from_slice(&response[108..128]);
    let lp_locker_address = Address::from_slice(&response[140..160]);

    Ok(OrchestratorState {
        cat_address,
        platform_token_active,
        hook_address,
        usdc_address,
        lp_locker_address,
    })
}

/// Result of graduation check for multiple tokens
#[derive(Debug, Clone)]
pub struct GraduationCheckResult {
    pub token: Address,
    pub is_graduated: bool,
}

/// Batch check if multiple tokens are graduated
///
/// Reduces N RPC calls to 1.
pub async fn batch_graduation_checks<T, P>(
    provider: &P,
    orchestrator: Address,
    tokens: &[Address],
) -> Result<Vec<GraduationCheckResult>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let chunks = super::chunk_batch(tokens, crate::BATCH_CHUNK_SIZE);
    let mut all_results = Vec::with_capacity(tokens.len());

    for chunk in chunks {
        let calls: Vec<CallSpec> = chunk
            .iter()
            .map(|token| CallSpec {
                target: orchestrator,
                calldata: Bytes::from(
                    GraduationOrchestratorV4::isGraduatedCall { token: *token }.abi_encode()
                ),
                return_size: 32, // bool
                use_call: false,
            })
            .collect();

        let response = super::execute_batch(provider, &calls).await?;

        for (i, token) in chunk.iter().enumerate() {
            let offset = i * 32;
            let is_graduated = if offset + 32 <= response.len() {
                U256::from_be_slice(&response[offset..offset + 32]) != U256::ZERO
            } else {
                false
            };

            all_results.push(GraduationCheckResult {
                token: *token,
                is_graduated,
            });
        }
    }

    Ok(all_results)
}

/// Result of agent authorization check
#[derive(Debug, Clone)]
pub struct AgentAuthResult {
    pub agent: Address,
    pub is_authorized: bool,
}

/// Batch check if multiple agents are authorized
pub async fn batch_agent_auth_checks<T, P>(
    provider: &P,
    orchestrator: Address,
    agents: &[Address],
) -> Result<Vec<AgentAuthResult>, EmasmError>
where
    T: Transport + Clone,
    P: Provider<T, Ethereum>,
{
    if agents.is_empty() {
        return Ok(Vec::new());
    }

    let chunks = super::chunk_batch(agents, crate::BATCH_CHUNK_SIZE);
    let mut all_results = Vec::with_capacity(agents.len());

    for chunk in chunks {
        let calls: Vec<CallSpec> = chunk
            .iter()
            .map(|agent| CallSpec {
                target: orchestrator,
                calldata: Bytes::from(
                    GraduationOrchestratorV4::authorizedAgentsCall { agent: *agent }.abi_encode()
                ),
                return_size: 32, // bool
                use_call: false,
            })
            .collect();

        let response = super::execute_batch(provider, &calls).await?;

        for (i, agent) in chunk.iter().enumerate() {
            let offset = i * 32;
            let is_authorized = if offset + 32 <= response.len() {
                U256::from_be_slice(&response[offset..offset + 32]) != U256::ZERO
            } else {
                false
            };

            all_results.push(AgentAuthResult {
                agent: *agent,
                is_authorized,
            });
        }
    }

    Ok(all_results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestrator_calldata_encoding() {
        let calldata = GraduationOrchestratorV4::catCall {}.abi_encode();
        // Should be just 4 bytes selector (no params)
        assert_eq!(calldata.len(), 4);
    }

    #[test]
    fn test_is_graduated_calldata() {
        let token = Address::repeat_byte(0x42);
        let calldata = GraduationOrchestratorV4::isGraduatedCall { token }.abi_encode();
        // 4 bytes selector + 32 bytes address
        assert_eq!(calldata.len(), 36);
    }
}
