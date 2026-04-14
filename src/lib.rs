//! emasm-service: Batch RPC calls using EVM bytecode assembly
//!
//! This crate provides optimized batch read operations for the 402.cat platform,
//! reducing RPC calls from 81+ to ~5 by bundling multiple reads into single eth_call.

mod batches;
mod bytecode;
mod error;

pub use batches::*;
pub use error::EmasmError;

/// Maximum items per batch before chunking (respects RPC gas limits)
pub const BATCH_CHUNK_SIZE: usize = 50;
