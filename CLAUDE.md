# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build --release        # optimized build (LTO enabled)
cargo run --release           # run latency benchmark
cargo run                     # debug build (faster compile, slower runtime)
cargo check                   # type-check without building
cargo clippy                  # lint
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

## Architecture

Single file (`src/main.rs`, ~690 lines). Key sections:

- **EIP-712 types** (lines 21-45): `sol!` macro defines `RegisterSigner`, `VerifySigner`, `VerifyWitness` structs for on-chain signature verification
- **Order encoding** (lines 202-241): Bitpacked order hash and cancel-all hash construction matching the RISEx smart contract format
- **`RiseClient`** (lines 244-508): Stateful API client holding HTTP client, signing keys, EIP-712 domain separator, and router target address. Handles signer registration, nonce management, permit signing, and timed order operations
- **`main`** (lines 574-690): Orchestrates benchmark rounds with 3-second nonce settle delays between operations

## Signing Flow

Every order/cancel requires a **permit** (EIP-712 `VerifyWitness` signature):
1. Fetch nonce state from API
2. Encode action hash (order params or cancel-all)
3. Sign `VerifyWitness` struct with signer key (base64-encoded)
4. Submit action + permit to API

Session signer must be registered first via `RegisterSigner` (signed by account key) + `VerifySigner` (signed by signer key).
