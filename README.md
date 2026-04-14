# emasm-service

**Batch EVM RPC calls using bytecode assembly.**

A Rust crate that bundles multiple on-chain read operations into a single `eth_call` by generating minimal EVM bytecode at runtime. Works on any EVM-compatible blockchain.

## Why

Every `eth_call` is a network round-trip. If you need 30 token balances, that's 30 calls. This crate generates EVM bytecode that executes all 30 `STATICCALL`s in sequence, packs the results contiguously in memory, and returns them in a single `RETURN`. One round-trip instead of 30.

| Operation | Without batching | With emasm | Reduction |
|---|---|---|---|
| Wallet balances (10 wallets × 3 tokens) | 30 calls | 1 call | **96.7%** |
| Token metadata (name, symbol, decimals, supply) | 4N calls | 1 call | **75%** |
| Uniswap V3 quotes (3 fee tiers) | 3 calls | 1 call | **66.7%** |
| Uniswap V4 quotes (pool checks + quotes) | 10-40 calls | 2 calls | **80-95%** |
| Typical endpoint | 81+ calls | ~5 calls | **~94%** |

## Quick Start

```rust
use alloy::providers::ProviderBuilder;
use alloy::primitives::Address;
use emasm_service::batch_wallet_balances;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = ProviderBuilder::new()
        .on_http("https://your-rpc-url.com".parse()?);

    let wallets: Vec<Address> = vec![
        "0x1234...".parse()?,
        "0x5678...".parse()?,
    ];
    let tokens: Vec<Address> = vec![
        "0xA0b8...".parse()?,  // USDC (or any ERC20)
        "0xdAC1...".parse()?,  // USDT (or any ERC20)
    ];

    let results = batch_wallet_balances(&provider, &wallets, &tokens).await?;

    for r in results {
        println!("{} has {} of token {}", r.wallet, r.balance, r.token);
    }

    Ok(())
}
```

## How It Works

```
┌─────────────────────┐     ┌──────────────────────────────┐     ┌─────────────┐     ┌─────────────────┐
│  1. Build CallSpecs │     │  2. Generate EVM Bytecode    │     │ 3. eth_call │     │   4. Decode     │
│                     │     │                              │     │             │     │                 │
│  target   : 0xA0b8  │     │  PUSH calldata  -> MSTORE    │     │  Single     │     │  Parse packed   │
│  calldata : 0x70a0  │ ──> │  PUSH target    -> STATICCALL│ ──> │  RPC call   │ ──> │  32-byte words  │
│  ret_size : 32      │     │  ... repeat for each call    │     │  (gas-free) │     │  into structs   │
│  use_call : false   │     │  RETURN packed results       │     │             │     │                 │
└─────────────────────┘     └──────────────────────────────┘     └─────────────┘     └─────────────────┘
```

The core primitive is `CallSpec`:

```rust
use emasm_service::bytecode::CallSpec;
use alloy::primitives::{Address, Bytes};

let call = CallSpec {
    target: token_address,              // Contract to call
    calldata: Bytes::from(encoded_abi), // ABI-encoded function call
    return_size: 32,                    // Expected return size in bytes
    use_call: false,                    // false = STATICCALL, true = CALL
};
```

- **STATICCALL** (default): For pure view functions. Cheaper, safer.
- **CALL**: For contracts that do internal state simulation (e.g., Uniswap V4 Quoter). Still read-only in `eth_call` context.

## Available Batch Operations

### Chain-Agnostic (works on any EVM chain)

| Function | What it does | Calls saved |
|---|---|---|
| `batch_wallet_balances` | ERC20 `balanceOf` for N wallets × M tokens | N×M → 1 |
| `batch_eth_balances` | Native ETH balances (parallel futures) | N → N (parallel) |
| `batch_erc20_metadata` | name, symbol, decimals, totalSupply | 4N → 1 |
| `batch_token_decimals` | Just decimals (lightweight) | N → 1 |
| `batch_token_supplies` | Just totalSupply | N → 1 |
| `batch_erc3009_prereqs` | ERC-3009 payment verification prereqs | 4 → 1 |
| `batch_token_metadata` | Token name + version (for EIP-712) | 2 → 1 |

### Uniswap V3 (any chain with V3 deployments)

| Function | What it does | Calls saved |
|---|---|---|
| `batch_v3_quotes` | Quote all fee tiers (500, 3000, 10000 bps) | 3 → 1 |
| `batch_v3_quotes_multi` | Multiple custom quote requests | N → 1 |
| `find_best_v3_quote` | Pick highest output from results | — |

### Uniswap V4 (any chain with V4 deployments)

| Function | What it does | Calls saved |
|---|---|---|
| `batch_v4_quotes` | Multiple pool key + direction quotes | N → 1 |
| `batch_multihop_quote` | Two-hop quote (e.g., USDC→X→Y) | 4 → 2 |
| `batch_pool_states` | V4 pool slot0 + liquidity | 2N → 1 |
| `discover_pool` | On-chain pool discovery by convention | 2 → 1 |
| `batch_discover_pools` | Batch pool discovery | 2N → 1 |
| `discover_token_pools` | Find both possible pool pairings | 4 → 1 |

### Position & Fee Queries

| Function | What it does | Calls saved |
|---|---|---|
| `batch_position_search` | Search position ownership + liquidity | 2N → 1 |
| `find_owned_position_with_liquidity` | Find first owned position with liquidity | 2N → 1 |
| `batch_fee_queries` | LP locker isLocked + claimable fees | 2N → 1 |

### Identity (ERC-8004)

| Function | What it does | Calls saved |
|---|---|---|
| `batch_resolve_by_name` | Resolve identities from registry | N → 1 |
| `batch_check_availability` | Check name availability | N → 1 |

### Unified Quoter

| Function | What it does | Calls saved |
|---|---|---|
| `get_batched_swap_quotes` | V3 + V4 quotes + metadata in one call | 20-50 → ~4 |

## Using on a New EVM Chain

The core bytecode engine (`CallSpec` + `build_batch_bytecode` + `execute_batch`) is **completely chain-agnostic**. It generates raw EVM bytecode — any chain that supports `eth_call` works.

For the higher-level batch functions:

1. **ERC20 operations** (`batch_wallet_balances`, `batch_erc20_metadata`, etc.) work on any chain with standard ERC20 tokens.

2. **Uniswap V3/V4 operations** require the Uniswap contracts to be deployed on your chain. Update the addresses in `swap_quoter.rs`:
   - `v3_quoter_address()` — V3 QuoterV2 address
   - `v4_quoter_address()` — V4 Quoter address
   - `universal_router_address()` — Universal Router address
   - `weth_address()` — Wrapped native token address

3. **Protocol-specific operations** (`batch_orchestrator_state`, pool discovery, etc.) are tied to specific contract ABIs. Replace with your own contract interfaces as needed.

### Custom Batch Operations

Build your own batches using the core primitives:

```rust
use emasm_service::bytecode::{CallSpec, build_batch_bytecode};
use alloy::primitives::{Address, Bytes};
use alloy::sol;
use alloy::sol_types::SolCall;

sol! {
    contract MyContract {
        function getValue() external view returns (uint256);
        function getOwner() external view returns (address);
    }
}

// Build custom call specs
let calls = vec![
    CallSpec {
        target: my_contract_address,
        calldata: Bytes::from(MyContract::getValueCall {}.abi_encode()),
        return_size: 32,
        use_call: false,
    },
    CallSpec {
        target: my_contract_address,
        calldata: Bytes::from(MyContract::getOwnerCall {}.abi_encode()),
        return_size: 32,
        use_call: false,
    },
];

// Generate bytecode and execute
let bytecode = build_batch_bytecode(&calls);
// Use with provider.call() — see batches/mod.rs execute_batch for reference
```

## Technical Details

### Memory Layout

The generated bytecode stores calldata and results in separate memory regions:

1. **Calldata region**: Each call's ABI-encoded input, padded to 32-byte boundaries
2. **Result region**: All return values packed contiguously, 32-byte aligned
3. A single `RETURN` sends back the entire result region

### Chunking

Large batches auto-chunk at 50 items (`BATCH_CHUNK_SIZE`) to respect RPC gas limits. Domain-specific functions adjust this based on per-item return size.

### Error Handling

Failed contract calls return zero bytes (the bytecode POPs the success flag). Callers must validate results — a zero balance could be genuine or a failed call. The `EmasmError` enum covers RPC failures, encoding errors, and batch size limits.

## Dependencies

- [alloy](https://github.com/alloy-rs/alloy) — Ethereum primitives, ABI, providers
- [tokio](https://tokio.rs) — Async runtime
- [futures](https://docs.rs/futures) — Async utilities
- [thiserror](https://docs.rs/thiserror) — Error handling
- [tracing](https://docs.rs/tracing) — Structured logging
- [hex](https://docs.rs/hex) — Hex encoding

## License

MIT
