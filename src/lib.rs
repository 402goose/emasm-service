//! emasm-service: Batch RPC calls using EVM bytecode assembly
//!
//! This crate provides optimized batch read operations for EVM-compatible chains,
//! reducing RPC calls from 80+ to ~5 by bundling multiple reads into a single eth_call.

mod batches;
mod bytecode;
mod error;

pub use batches::*;
pub use error::EmasmError;

/// Maximum items per batch before chunking (respects RPC gas limits)
pub const BATCH_CHUNK_SIZE: usize = 50;
