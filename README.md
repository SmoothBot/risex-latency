# risex-latency

Latency benchmark for the [RISEx](https://rise.trade) perpetual futures exchange API. Measures end-to-end order submission times including nonce fetch, EIP-712 signing, and HTTP round-trip.

## Setup

1. Install [Rust](https://rustup.rs/)
2. Create a `.env` file:

```env
ACCOUNT_PRIVATE_KEY=0x...
SIGNER_PRIVATE_KEY=0x...
API_URL=https://api.testnet.rise.trade   # optional, this is the default
```

3. Run the benchmark:

```bash
cargo run --release
```

## What It Measures

Runs 10 rounds of each operation against the configured RISEx API:

| Test | Operations |
|------|-----------|
| Market orders | Market buy to open, market sell to close (reduce-only) |
| Limit orders | Post-only limit buy (far from market), cancel-all |

Each round reports timing breakdown:
- **nonce** - API call to fetch current nonce state
- **sign** - EIP-712 permit construction and ECDSA signing (local CPU)
- **submit** - HTTP POST to place/cancel endpoint

Final summary shows submit latency statistics (avg, p50, p95, min, max) across all rounds.

## Example Output

```
== Market Orders: 10 rounds ==

  #1 | open:   45.2ms | close:   38.7ms
  #2 | open:   42.1ms | close:   41.3ms
  ...

==============================================================
  SUBMIT LATENCY SUMMARY (ms)
==============================================================
                    avg     p50     p95     min     max
  ----------------------------------------------------
  mkt buy           43.2    42.1    48.5    38.2    52.1
  mkt sell          39.8    39.2    45.1    35.6    47.3
  lmt place         41.5    40.8    47.2    36.9    50.4
  lmt cancel        37.2    36.5    42.8    33.1    45.7
  ----------------------------------------------------
  n = 10 rounds per type
==============================================================
```

*(Values are illustrative)*
