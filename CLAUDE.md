# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build --release        # optimized build (LTO enabled)
cargo run --release           # run latency benchmark (src/main.rs)
cargo run                     # debug build (faster compile, slower runtime)
cargo check                   # type-check without building
cargo clippy                  # lint

# WebSocket diagnostic binaries (in src/bin/)
cargo run --bin ws_orderbook                # orderbook stream with checksum validation
cargo run --bin ws_checksum_debug           # checksum algorithm debugging (staging)
cargo run --bin ws_logindex_monitor         # multi-connection log_index rollback detector (prod)
cargo run --bin ws_logindex_monitor_staging # single-connection log_index rollback detector (staging)
```

No tests exist yet. No CI configured.

## Environment Variables

Loaded from `.env` and `../.env` (parent directory) via dotenvy:

- `ACCOUNT_ADDRESS` (required if no `ACCOUNT_PRIVATE_KEY`) - Ethereum address of the trading account
- `ACCOUNT_PRIVATE_KEY` (optional) - Ethereum private key hex for the trading account; only needed if the signer is not yet registered
- `SIGNER_PRIVATE_KEY` (required) - Ethereum private key hex for the session signer
- `API_URL` (optional) - RISEx API base URL, defaults to `https://api.testnet.rise.trade`

## What This Does

Single-binary latency benchmark for the RISEx perpetual futures exchange API. Runs 10 rounds each of:

1. **Market orders** - open (market buy) then close position (market sell, reduce-only)
2. **Limit post-only orders** - place far-from-market then cancel-all

Reports per-round breakdown (nonce fetch, EIP-712 signing, HTTP submit) and aggregate submit latency stats (avg, p50, p95, min, max).

Additionally includes WebSocket diagnostic tools in `src/bin/` for monitoring orderbook data integrity (checksum validation, log_index rollback detection).

## Architecture

**Main benchmark** (`src/main.rs`, ~720 lines):

- **EIP-712 types** (~line 22): `sol!` macro defines `RegisterSigner`, `VerifySigner`, `VerifyWitness` structs for on-chain signature verification
- **Order encoding** (~line 202): Bitpacked order hash and cancel-all hash construction matching the RISEx smart contract format
- **`RiseClient`** (~line 244): Stateful API client holding HTTP client, signing keys, EIP-712 domain separator, and router target address. Handles signer registration, nonce management, permit signing, and timed order operations
- **`main`** (~line 594): Orchestrates benchmark rounds with 3-second nonce settle delays between operations

**WebSocket tools** (`src/bin/`):

- `ws_logindex_monitor.rs` - Spawns 10 concurrent WS connections to prod, detects log_index rollbacks across markets, halts and dumps debug JSON on anomaly
- `ws_logindex_monitor_staging.rs` - Single-connection variant targeting staging
- `ws_checksum_debug.rs` - Tests multiple CRC32 checksum algorithms against orderbook updates to reverse-engineer the checksum format
- `ws_orderbook.rs` - Streams all markets' orderbook data, logs block_number/log_index/checksum per message

Note: `src/bin/Untitled/` contains duplicate copies of the bin files (likely stale).

## Signing Flow

Every order/cancel requires a **permit** (EIP-712 `VerifyWitness` signature):
1. Fetch nonce state from API
2. Encode action hash (order params or cancel-all)
3. Sign `VerifyWitness` struct with signer key (base64-encoded)
4. Submit action + permit to API

Session signer must be registered first via `RegisterSigner` (signed by account key) + `VerifySigner` (signed by signer key).
