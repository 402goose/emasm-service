//! EVM bytecode generation for batch operations
//!
//! Generates minimal bytecode that executes multiple CALL or STATICCALL operations
//! and returns packed results. Used with eth_call for gas-free batch reads.
//!
//! # CALL vs STATICCALL
//!
//! By default, STATICCALL is used which is appropriate for pure view functions.
//! However, some contracts like the Uniswap V4 Quoter perform internal state
//! simulation and require CALL (even though eth_call context is read-only).
//! Use `CallSpec.use_call = true` for such cases.

use alloy::primitives::{Address, Bytes, U256};

/// EVM opcodes used in batch bytecode
mod opcodes {
    pub const PUSH1: u8 = 0x60;
    pub const PUSH2: u8 = 0x61;
    pub const PUSH4: u8 = 0x63;
    pub const PUSH20: u8 = 0x73;
    pub const PUSH32: u8 = 0x7f;
    pub const DUP1: u8 = 0x80;
    pub const DUP2: u8 = 0x81;
    pub const DUP3: u8 = 0x82;
    pub const DUP4: u8 = 0x83;
    pub const MSTORE: u8 = 0x52;
    pub const MLOAD: u8 = 0x51;
    pub const CALL: u8 = 0xf1;
    pub const STATICCALL: u8 = 0xfa;
    pub const RETURN: u8 = 0xf3;
    pub const ADD: u8 = 0x01;
    pub const GAS: u8 = 0x5a;
    pub const POP: u8 = 0x50;
}

/// Single call specification for batch assembly
pub struct CallSpec {
    pub target: Address,
    pub calldata: Bytes,
    pub return_size: usize,
    /// Use CALL instead of STATICCALL (needed for contracts that do internal state simulation like V4 Quoter)
    pub use_call: bool,
}

/// Build bytecode that executes multiple calls and returns packed results
///
/// Layout:
/// 1. For each call: store calldata in memory, execute CALL or STATICCALL
/// 2. Pack all return values contiguously in memory
/// 3. RETURN the packed result
///
/// Uses STATICCALL by default, or CALL if `CallSpec.use_call` is true.
pub fn build_batch_bytecode(calls: &[CallSpec]) -> Bytes {
    let mut bytecode = Vec::with_capacity(calls.len() * 100);
    let mut memory_offset: usize = 0;
    let mut result_offset: usize = 0;

    // Calculate where results will be stored (after all calldata)
    // IMPORTANT: Must use the PADDED calldata size (rounded up to 32 bytes per call)
    // to match how memory_offset is actually incremented
    let calldata_total: usize = calls
        .iter()
        .map(|c| ((c.calldata.len() + 31) / 32) * 32)
        .sum();
    let result_start = calldata_total; // Already aligned since each chunk is 32-byte aligned

    for call in calls {
        // Store calldata in memory
        let calldata_offset = memory_offset;
        for (i, chunk) in call.calldata.chunks(32).enumerate() {
            // PUSH32 <chunk> PUSH1 <offset> MSTORE
            if chunk.len() == 32 {
                bytecode.push(opcodes::PUSH32);
                bytecode.extend_from_slice(chunk);
            } else {
                // Pad shorter chunk
                let mut padded = [0u8; 32];
                padded[..chunk.len()].copy_from_slice(chunk);
                bytecode.push(opcodes::PUSH32);
                bytecode.extend_from_slice(&padded);
            }
            let offset = calldata_offset + i * 32;
            push_value(&mut bytecode, offset);
            bytecode.push(opcodes::MSTORE);
        }
        memory_offset += ((call.calldata.len() + 31) / 32) * 32;

        // Execute CALL or STATICCALL
        // CALL(gas, addr, value, argsOffset, argsLen, retOffset, retLen) - 7 args
        // STATICCALL(gas, addr, argsOffset, argsLen, retOffset, retLen) - 6 args
        let ret_offset = result_start + result_offset;

        // Push arguments in reverse order
        push_value(&mut bytecode, call.return_size); // retLen
        push_value(&mut bytecode, ret_offset); // retOffset
        push_value(&mut bytecode, call.calldata.len()); // argsLen
        push_value(&mut bytecode, calldata_offset); // argsOffset

        if call.use_call {
            // For CALL, we need to push value (0) before address
            push_value(&mut bytecode, 0); // value = 0
        }

        // Push target address
        bytecode.push(opcodes::PUSH20);
        bytecode.extend_from_slice(call.target.as_slice());

        // GAS for available gas
        bytecode.push(opcodes::GAS);

        // CALL or STATICCALL
        if call.use_call {
            bytecode.push(opcodes::CALL);
        } else {
            bytecode.push(opcodes::STATICCALL);
        }

        // POP the success flag (we handle failures via zero values)
        bytecode.push(opcodes::POP);

        result_offset += ((call.return_size + 31) / 32) * 32;
    }

    // RETURN all results
    let total_result_size = result_offset;
    push_value(&mut bytecode, total_result_size); // size
    push_value(&mut bytecode, result_start); // offset
    bytecode.push(opcodes::RETURN);

    Bytes::from(bytecode)
}

/// Push a value onto the stack using minimal bytes
fn push_value(bytecode: &mut Vec<u8>, value: usize) {
    if value == 0 {
        bytecode.push(opcodes::PUSH1);
        bytecode.push(0x00);
    } else if value <= 0xFF {
        bytecode.push(opcodes::PUSH1);
        bytecode.push(value as u8);
    } else if value <= 0xFFFF {
        bytecode.push(opcodes::PUSH2);
        bytecode.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= 0xFFFFFFFF {
        bytecode.push(opcodes::PUSH4);
        bytecode.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        // Use full 32 bytes for large values
        bytecode.push(opcodes::PUSH32);
        bytecode.extend_from_slice(&U256::from(value).to_be_bytes::<32>());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_value_zero() {
        let mut bytecode = Vec::new();
        push_value(&mut bytecode, 0);
        assert_eq!(bytecode, vec![0x60, 0x00]); // PUSH1 0x00
    }

    #[test]
    fn test_push_value_small() {
        let mut bytecode = Vec::new();
        push_value(&mut bytecode, 32);
        assert_eq!(bytecode, vec![0x60, 0x20]); // PUSH1 0x20
    }

    #[test]
    fn test_push_value_medium() {
        let mut bytecode = Vec::new();
        push_value(&mut bytecode, 256);
        assert_eq!(bytecode, vec![0x61, 0x01, 0x00]); // PUSH2 0x0100
    }
}
