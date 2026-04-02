use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::{eip712_domain, sol, SolStruct};
use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ── Constants ──────────────────────────────────────────────
const ROUNDS: usize = 10;
const MAX_NONCE_RETRIES: usize = 3;
const MARKET_SYMBOL: &str = "ETH/USDC";
const PERMIT_DEADLINE_SECS: u64 = 604800;
const SIGNER_EXPIRY_SECS: u64 = 30 * 24 * 3600;
const REGISTER_MSG: &str = "Registering signer for RISEx";

const ACTION_PLACE_ORDER: &str = "RISE_PERPS_PLACE_ORDER_V1";
const ACTION_CANCEL_ALL_ORDERS: &str = "RISE_PERPS_CANCEL_ALL_ORDERS_V1";

const V3_FLAG_PERMIT: u8 = 0x01;

// ── EIP-712 types ──────────────────────────────────────────
sol! {
    struct RegisterSigner {
        address account;
        address signer;
        string message;
        uint32 expiration;
        uint48 nonceAnchor;
        uint8 nonceBitmap;
    }

    struct VerifySigner {
        address account;
        uint48 nonceAnchor;
        uint8 nonceBitmap;
    }

    struct VerifyWitness {
        address account;
        address target;
        bytes32 hash;
        uint48 nonceAnchor;
        uint8 nonceBitmap;
        uint32 deadline;
    }
}

// ── Timing breakdown ───────────────────────────────────────
#[derive(Clone)]
struct Timing {
    nonce_ms: f64,
    sign_ms: f64,
    submit_ms: f64,
    total_ms: f64,
}

impl Timing {
    fn print(&self, label: &str) {
        println!(
            "         {}: nonce {:5.1}ms | sign {:5.1}ms | submit {:5.1}ms | total {:5.1}ms",
            label, self.nonce_ms, self.sign_ms, self.submit_ms, self.total_ms
        );
    }
}

// ── API types ──────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    data: Option<T>,
    #[allow(dead_code)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Eip712DomainResp {
    name: String,
    version: String,
    #[serde(deserialize_with = "de_chain_id")]
    chain_id: u64,
    verifying_contract: String,
}

fn de_chain_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match &v {
        serde_json::Value::Number(n) => {
            n.as_u64().ok_or_else(|| serde::de::Error::custom("bad chain_id"))
        }
        serde_json::Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom("bad chain_id")),
    }
}

#[derive(Debug, Deserialize)]
struct SystemConfig { addresses: SystemAddresses }
#[derive(Debug, Deserialize)]
struct SystemAddresses { router: Option<String>, perp_v2: Option<PerpV2> }
#[derive(Debug, Deserialize)]
struct PerpV2 { orders_manager: Option<String> }

#[derive(Debug, Deserialize)]
struct Market {
    market_id: serde_json::Value,
    display_name: String,
    visible: bool,
    last_price: String,
    config: MarketConfig,
}
#[derive(Debug, Deserialize)]
struct MarketConfig { min_order_size: String, step_size: String, step_price: String }
#[derive(Debug, Deserialize)]
struct MarketsResp { markets: Vec<Market> }
#[derive(Debug, Deserialize)]
struct BalanceResp { balance: String }
#[derive(Debug, Deserialize)]
struct PositionResp { position: Option<Position> }
#[derive(Debug, Deserialize)]
struct Position { size: String, side: u8 }
#[derive(Debug, Deserialize)]
struct SessionKeyStatusResp { status: u8 }
#[derive(Debug, Deserialize)]
struct NonceState { nonce_anchor: String, current_bitmap_index: u8 }

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OrderResponse { order_id: String, sc_order_id: Option<String>, tx_hash: String }

#[derive(Debug, Deserialize)]
struct OrderbookResp { bids: Vec<OrderbookLevel>, #[allow(dead_code)] asks: Vec<OrderbookLevel> }
#[derive(Debug, Deserialize)]
struct OrderbookLevel { price: String, #[allow(dead_code)] quantity: String }

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CancelResponse { success: bool, tx_hash: String }

// ── Request types ──────────────────────────────────────────
#[derive(Serialize)]
struct PermitParams {
    account: String, signer: String,
    nonce_anchor: u64, nonce_bitmap_index: u8,
    deadline: u64, signature: String,
}

#[derive(Serialize)]
struct PlaceOrderReq {
    market_id: u32, side: u8, order_type: u8,
    price_ticks: u32, size_steps: u32, time_in_force: u8,
    post_only: bool, reduce_only: bool, stp_mode: u8,
    ttl_units: u16, client_order_id: String, builder_id: u16,
    permit: PermitParams,
}

#[derive(Serialize)]
struct RegisterSignerReq {
    account: String, signer: String, message: String,
    nonce_anchor: String, nonce_bitmap_index: u8,
    expiration: String,
    account_signature: String, signer_signature: String,
    label: String,
}

// ── Signing helpers ────────────────────────────────────────
fn signing_key_from_hex(hex_str: &str) -> SigningKey {
    let bytes = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str)).expect("invalid hex key");
    SigningKey::from_bytes((&bytes[..]).into()).expect("invalid key")
}

fn address_from_key(key: &SigningKey) -> Address {
    let pubkey = key.verifying_key();
    let pubkey_bytes = pubkey.to_encoded_point(false);
    let hash = keccak256(&pubkey_bytes.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

fn sign_hash(key: &SigningKey, hash: &[u8; 32]) -> [u8; 65] {
    let (sig, recid) = key.sign_prehash(hash).expect("signing failed");
    let mut bytes = [0u8; 65];
    bytes[..64].copy_from_slice(&sig.to_bytes());
    let v = recid.to_byte();
    bytes[64] = if v < 27 { v + 27 } else { v };
    bytes
}

fn sign_hash_hex(key: &SigningKey, hash: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(sign_hash(key, hash)))
}

fn sign_hash_base64(key: &SigningKey, hash: &[u8; 32]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(sign_hash(key, hash))
}

fn eip712_hash(domain_separator: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    let mut data = Vec::with_capacity(66);
    data.extend_from_slice(b"\x19\x01");
    data.extend_from_slice(domain_separator);
    data.extend_from_slice(struct_hash);
    keccak256(&data).0
}

// ── Order encoding ─────────────────────────────────────────
fn encode_order_hash(
    market_id: u32, size_steps: u32, price_ticks: u32,
    side: u8, order_type: u8, time_in_force: u8,
    post_only: bool, reduce_only: bool, stp_mode: u8,
) -> B256 {
    let mut order_flags: u8 = 0;
    if side & 1 != 0 { order_flags |= 0x01; }
    if post_only { order_flags |= 0x02; }
    if reduce_only { order_flags |= 0x04; }
    order_flags |= (stp_mode & 3) << 3;
    order_flags |= (order_type & 1) << 5;
    order_flags |= (time_in_force & 3) << 6;

    let mut data = U256::ZERO;
    data |= U256::from(market_id as u64 & 0xFFFF) << 70;
    data |= U256::from(size_steps as u64 & 0xFFFFFFFF) << 38;
    data |= U256::from(price_ticks as u64 & 0xFFFFFF) << 14;
    data |= U256::from(order_flags) << 6;
    data |= U256::from(1u8 << 1);

    let action_hash = keccak256(ACTION_PLACE_ORDER.as_bytes());

    let mut encoded = Vec::with_capacity(192);
    encoded.extend_from_slice(&action_hash.0);
    encoded.extend_from_slice(&U256::from(V3_FLAG_PERMIT).to_be_bytes::<32>());
    encoded.extend_from_slice(&data.to_be_bytes::<32>());
    encoded.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    encoded.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    encoded.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());

    keccak256(&encoded)
}

fn encode_cancel_all_hash(market_id: u32) -> B256 {
    let action_hash = keccak256(ACTION_CANCEL_ALL_ORDERS.as_bytes());
    let mut encoded = Vec::with_capacity(64);
    encoded.extend_from_slice(&action_hash.0);
    encoded.extend_from_slice(&U256::from(market_id).to_be_bytes::<32>());
    keccak256(&encoded)
}

// ── RISEx client ───────────────────────────────────────────
struct RiseClient {
    http: Client,
    base_url: String,
    account_key: Option<SigningKey>,
    signer_key: SigningKey,
    account: Address,
    signer_addr: Address,
    domain_separator: [u8; 32],
    target: Address,
}

impl RiseClient {
    async fn new(base_url: &str, account: Address, account_key: Option<SigningKey>, signer_hex: &str) -> Self {
        let http = Client::new();
        let signer_key = signing_key_from_hex(signer_hex);
        let signer_addr = address_from_key(&signer_key);

        let mut client = Self {
            http, base_url: base_url.to_string(),
            account_key, signer_key, account, signer_addr,
            domain_separator: [0u8; 32], target: Address::ZERO,
        };
        client.init().await;
        client
    }

    async fn init(&mut self) {
        let resp: ApiResponse<Eip712DomainResp> = self.http
            .get(format!("{}/v1/auth/eip712-domain", self.base_url))
            .send().await.unwrap().json().await.unwrap();
        let d = resp.data.expect("no domain data");
        let domain = eip712_domain! {
            name: d.name, version: d.version, chain_id: d.chain_id,
            verifying_contract: d.verifying_contract.parse::<Address>().unwrap(),
        };
        self.domain_separator = domain.hash_struct().0;

        let resp: ApiResponse<SystemConfig> = self.http
            .get(format!("{}/v1/system/config", self.base_url))
            .send().await.unwrap().json().await.unwrap();
        let config = resp.data.expect("no config");
        let target_str = config.addresses.router
            .or_else(|| config.addresses.perp_v2.and_then(|p| p.orders_manager))
            .expect("no router/orders_manager in config");
        self.target = target_str.parse().expect("invalid target address");
    }

    async fn get_nonce_state(&self) -> NonceState {
        let resp: ApiResponse<NonceState> = self.http
            .get(format!("{}/v1/nonce-state/{}", self.base_url, self.account))
            .send().await.unwrap().json().await.unwrap();
        resp.data.expect("no nonce state")
    }

    async fn register_signer(&self) {
        let resp: ApiResponse<SessionKeyStatusResp> = self.http
            .get(format!("{}/v1/auth/session-key-status?account={}&signer={}",
                self.base_url, self.account, self.signer_addr))
            .send().await.unwrap().json().await.unwrap();
        if resp.data.map(|d| d.status).unwrap_or(0) == 1 {
            println!("  signer already registered, skipping");
            return;
        }

        let account_key = self.account_key.as_ref().unwrap_or_else(|| {
            panic!("Signer is not registered and ACCOUNT_PRIVATE_KEY is not set. \
                    Either register the signer first or provide ACCOUNT_PRIVATE_KEY.")
        });

        let nonce_state = self.get_nonce_state().await;
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let expiration = now_secs + SIGNER_EXPIRY_SECS;
        let auth_nonce_anchor: u64 = nonce_state.nonce_anchor.parse::<u64>().unwrap() + 1;

        let reg = RegisterSigner {
            account: self.account, signer: self.signer_addr,
            message: REGISTER_MSG.to_string(), expiration: expiration as u32,
            nonceAnchor: alloy_primitives::Uint::<48, 1>::from(auth_nonce_anchor),
            nonceBitmap: 0,
        };
        let digest = eip712_hash(&self.domain_separator, &reg.eip712_hash_struct().0);
        let account_sig = sign_hash_hex(account_key, &digest);

        let verify = VerifySigner {
            account: self.account,
            nonceAnchor: alloy_primitives::Uint::<48, 1>::from(auth_nonce_anchor),
            nonceBitmap: 0,
        };
        let digest = eip712_hash(&self.domain_separator, &verify.eip712_hash_struct().0);
        let signer_sig = sign_hash_hex(&self.signer_key, &digest);

        let body = RegisterSignerReq {
            account: format!("{}", self.account), signer: format!("{}", self.signer_addr),
            message: REGISTER_MSG.to_string(),
            nonce_anchor: auth_nonce_anchor.to_string(), nonce_bitmap_index: 0,
            expiration: expiration.to_string(),
            account_signature: account_sig, signer_signature: signer_sig,
            label: "rust-latency".to_string(),
        };
        let resp = self.http.post(format!("{}/v1/auth/register-signer", self.base_url))
            .json(&body).send().await.unwrap();
        if !resp.status().is_success() {
            panic!("register-signer failed: {}", resp.text().await.unwrap_or_default());
        }
    }

    /// Create permit with timing breakdown: returns (permit, nonce_ms, sign_ms)
    fn sign_permit(&self, hash: B256, nonce_state: &NonceState) -> (PermitParams, f64) {
        let t = Instant::now();

        let nonce_anchor: u64 = nonce_state.nonce_anchor.parse().unwrap();
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let deadline = now_secs + PERMIT_DEADLINE_SECS;

        let witness = VerifyWitness {
            account: self.account, target: self.target, hash,
            nonceAnchor: alloy_primitives::Uint::<48, 1>::from(nonce_anchor),
            nonceBitmap: nonce_state.current_bitmap_index,
            deadline: deadline as u32,
        };
        let digest = eip712_hash(&self.domain_separator, &witness.eip712_hash_struct().0);
        let signature = sign_hash_base64(&self.signer_key, &digest);

        let sign_ms = t.elapsed().as_secs_f64() * 1000.0;

        (PermitParams {
            account: format!("{}", self.account), signer: format!("{}", self.signer_addr),
            nonce_anchor, nonce_bitmap_index: nonce_state.current_bitmap_index,
            deadline, signature,
        }, sign_ms)
    }

    /// Place order with timing breakdown
    async fn place_order_timed(
        &self, market_id: u32, size_steps: u32, price_ticks: u32,
        side: u8, order_type: u8, time_in_force: u8,
        post_only: bool, reduce_only: bool, stp_mode: u8,
    ) -> (OrderResponse, Timing) {
        let total_start = Instant::now();

        // 1. Encode hash (cpu, negligible)
        let hash = encode_order_hash(
            market_id, size_steps, price_ticks, side, order_type,
            time_in_force, post_only, reduce_only, stp_mode,
        );

        for attempt in 0..MAX_NONCE_RETRIES {
            // 2. Fetch nonce
            let t = Instant::now();
            let nonce_state = self.get_nonce_state().await;
            let nonce_ms = t.elapsed().as_secs_f64() * 1000.0;

            // 3. Sign permit
            let (permit, sign_ms) = self.sign_permit(hash, &nonce_state);

            // 4. Submit order
            let t = Instant::now();
            let body = PlaceOrderReq {
                market_id, side, order_type, price_ticks, size_steps,
                time_in_force, post_only, reduce_only, stp_mode,
                ttl_units: 0, client_order_id: "0".to_string(), builder_id: 0,
                permit,
            };
            let resp = self.http.post(format!("{}/v1/orders/place", self.base_url))
                .json(&body).send().await.unwrap();
            let status = resp.status();
            let text = resp.text().await.unwrap();
            let submit_ms = t.elapsed().as_secs_f64() * 1000.0;

            if !status.is_success() {
                if text.contains("InvalidNonceIndex") && attempt < MAX_NONCE_RETRIES - 1 {
                    eprintln!("  [retry {}/{}] nonce collision, refetching...", attempt + 1, MAX_NONCE_RETRIES - 1);
                    continue;
                }
                panic!("place_order failed ({}): {}", status, text);
            }
            let api: ApiResponse<OrderResponse> =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse: {} body: {}", e, text));
            let order = api.data.unwrap_or_else(|| panic!("no order data: {}", text));

            let timing = Timing {
                nonce_ms, sign_ms, submit_ms,
                total_ms: total_start.elapsed().as_secs_f64() * 1000.0,
            };
            return (order, timing);
        }
        unreachable!()
    }

    async fn market_buy_timed(&self, market_id: u32, size_steps: u32) -> (OrderResponse, Timing) {
        self.place_order_timed(market_id, size_steps, 0, 0, 0, 3, false, false, 0).await
    }

    async fn market_sell_timed(&self, market_id: u32, size_steps: u32, reduce_only: bool) -> (OrderResponse, Timing) {
        self.place_order_timed(market_id, size_steps, 0, 1, 0, 3, false, reduce_only, 0).await
    }

    async fn limit_buy_timed(&self, market_id: u32, size_steps: u32, price_ticks: u32, post_only: bool) -> (OrderResponse, Timing) {
        self.place_order_timed(market_id, size_steps, price_ticks, 0, 1, 0, post_only, false, 0).await
    }

    /// Cancel all with timing breakdown
    async fn cancel_all_timed(&self, market_id: u32) -> (CancelResponse, Timing) {
        let total_start = Instant::now();

        let hash = encode_cancel_all_hash(market_id);

        for attempt in 0..MAX_NONCE_RETRIES {
            let t = Instant::now();
            let nonce_state = self.get_nonce_state().await;
            let nonce_ms = t.elapsed().as_secs_f64() * 1000.0;

            let (permit, sign_ms) = self.sign_permit(hash, &nonce_state);

            let t = Instant::now();
            let body = serde_json::json!({ "market_id": market_id, "permit": permit });
            let resp = self.http.post(format!("{}/v1/orders/cancel-all", self.base_url))
                .json(&body).send().await.unwrap();
            let status = resp.status();
            let text = resp.text().await.unwrap();
            let submit_ms = t.elapsed().as_secs_f64() * 1000.0;

            if !status.is_success() {
                if text.contains("InvalidNonceIndex") && attempt < MAX_NONCE_RETRIES - 1 {
                    eprintln!("  [retry {}/{}] nonce collision, refetching...", attempt + 1, MAX_NONCE_RETRIES - 1);
                    continue;
                }
                panic!("cancel_all failed ({}): {}", status, text);
            }
            let api: ApiResponse<CancelResponse> =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse: {} body: {}", e, text));
            let cancel = api.data.unwrap_or_else(|| panic!("no cancel data: {}", text));

            let timing = Timing {
                nonce_ms, sign_ms, submit_ms,
                total_ms: total_start.elapsed().as_secs_f64() * 1000.0,
            };
            return (cancel, timing);
        }
        unreachable!()
    }

    async fn close_position_timed(&self, market_id: u32, step_size: f64) -> Option<(OrderResponse, Timing, f64)> {
        let t = Instant::now();
        let pos_resp: ApiResponse<PositionResp> = self.http
            .get(format!("{}/v1/account/position?market_id={}&account={}",
                self.base_url, market_id, self.account))
            .send().await.unwrap().json().await.unwrap();
        let get_pos_ms = t.elapsed().as_secs_f64() * 1000.0;

        let pos = pos_resp.data.and_then(|d| d.position).filter(|p| p.size != "0")?;
        let size_f: f64 = pos.size.parse().unwrap();
        let size_steps = (size_f.abs() / step_size).round() as u32;

        let (order, timing) = if pos.side == 0 {
            self.market_sell_timed(market_id, size_steps, true).await
        } else {
            self.market_buy_timed(market_id, size_steps).await
        };
        Some((order, timing, get_pos_ms))
    }

    async fn get_markets(&self) -> Vec<Market> {
        let resp: ApiResponse<MarketsResp> = self.http
            .get(format!("{}/v1/markets", self.base_url))
            .send().await.unwrap().json().await.unwrap();
        resp.data.unwrap().markets
    }

    async fn get_balance(&self) -> String {
        let resp: ApiResponse<BalanceResp> = self.http
            .get(format!("{}/v1/account/cross-margin-balance?account={}",
                self.base_url, self.account))
            .send().await.unwrap().json().await.unwrap();
        resp.data.unwrap().balance
    }

    async fn get_orderbook(&self, market_id: u32, limit: u32) -> OrderbookResp {
        let resp: ApiResponse<OrderbookResp> = self.http
            .get(format!("{}/v1/orderbook?market_id={}&limit={}",
                self.base_url, market_id, limit))
            .send().await.unwrap().json().await.unwrap();
        resp.data.expect("no orderbook data")
    }
}

// ── Formatting / stats ─────────────────────────────────────
fn format_wad(wad: &str) -> String {
    if wad.contains('.') { return wad.to_string(); }
    let is_negative = wad.starts_with('-');
    let abs_str = if is_negative { &wad[1..] } else { wad };
    let padded = format!("{:0>19}", abs_str);
    let (integer, decimal) = padded.split_at(padded.len() - 18);
    let trimmed = decimal.trim_end_matches('0');
    let dec = if trimmed.is_empty() { "0" } else { trimmed };
    let sign = if is_negative { "-" } else { "" };
    format!("{}{}.{}", sign, integer, dec)
}

struct Stats {
    label: String,
    avg: f64,
    p50: f64,
    p95: f64,
    min: f64,
    max: f64,
    n: usize,
}

fn compute_stats(label: &str, timings: &[Timing]) -> Stats {
    let mut vals: Vec<f64> = timings.iter().map(|t| t.submit_ms).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = vals.len();
    Stats {
        label: label.to_string(),
        avg: vals.iter().sum::<f64>() / n as f64,
        p50: vals[n / 2],
        p95: vals[((n as f64 * 0.95) as usize).min(n - 1)],
        min: vals[0],
        max: vals[n - 1],
        n,
    }
}

fn print_summary(all: &[Stats]) {
    let w = 62;

    println!("\n{}", "=".repeat(w));
    println!("  SUBMIT LATENCY SUMMARY (ms)");
    println!("{}", "=".repeat(w));
    println!(
        "  {:<14} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "", "avg", "p50", "p95", "min", "max"
    );
    println!("  {}", "-".repeat(w - 2));

    for s in all {
        println!(
            "  {:<14} {:>6.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1}",
            s.label, s.avg, s.p50, s.p95, s.min, s.max
        );
    }

    println!("  {}", "-".repeat(w - 2));
    println!("  n = {} rounds per type", all[0].n);
    println!("{}\n", "=".repeat(w));
}

// ── Main ───────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    dotenvy::from_path("../.env").ok();
    dotenvy::dotenv().ok();

    let account_key_hex = std::env::var("ACCOUNT_PRIVATE_KEY").ok();
    let signer_key_hex = std::env::var("SIGNER_PRIVATE_KEY").expect("SIGNER_PRIVATE_KEY not set");
    let api_url = std::env::var("API_URL").unwrap_or_else(|_| "https://api.testnet.rise.trade".into());

    // Derive account address from ACCOUNT_ADDRESS or ACCOUNT_PRIVATE_KEY
    let account_key = account_key_hex.as_deref().map(signing_key_from_hex);
    let account: Address = match std::env::var("ACCOUNT_ADDRESS") {
        Ok(addr) => addr.parse().expect("invalid ACCOUNT_ADDRESS"),
        Err(_) => {
            let key = account_key.as_ref().unwrap_or_else(|| {
                panic!("Either ACCOUNT_ADDRESS or ACCOUNT_PRIVATE_KEY must be set")
            });
            address_from_key(key)
        }
    };

    let nonce_settle = std::time::Duration::from_secs(3);

    println!("Initializing client...");
    let t0 = Instant::now();
    let client = RiseClient::new(&api_url, account, account_key, &signer_key_hex).await;
    println!("  init: {:.1}ms", t0.elapsed().as_secs_f64() * 1000.0);

    let t1 = Instant::now();
    client.register_signer().await;
    println!("  registerSigner: {:.1}ms", t1.elapsed().as_secs_f64() * 1000.0);

    let markets = client.get_markets().await;
    let market = markets.iter()
        .find(|m| m.visible && m.display_name == MARKET_SYMBOL)
        .unwrap_or_else(|| panic!("Market {} not found", MARKET_SYMBOL));
    let market_id: u32 = match &market.market_id {
        serde_json::Value::Number(n) => n.as_u64().unwrap() as u32,
        serde_json::Value::String(s) => s.parse().unwrap(),
        _ => panic!("unexpected market_id type"),
    };

    let min_size: f64 = market.config.min_order_size.parse().unwrap();
    let step_size: f64 = market.config.step_size.parse().unwrap();
    let step_price: f64 = market.config.step_price.parse().unwrap();
    let min_size_steps = (min_size / step_size).round() as u32;

    let balance = client.get_balance().await;
    println!("\nAccount: {}", client.account);
    println!("Balance: {} USDC", format_wad(&balance));
    println!("Market:  {} (id={})", market.display_name, market_id);
    println!("Size:    {} ({} steps)", min_size, min_size_steps);
    println!("Step:    price={}  size={}", step_price, step_size);

    // ══════════════════════════════════════════════════════════
    // Test 1: Market orders (open + close)
    // ══════════════════════════════════════════════════════════
    println!("\n== Market Orders: {} rounds ==\n", ROUNDS);

    let mut mkt_open_timings = Vec::new();
    let mut mkt_close_timings = Vec::new();


    for i in 0..ROUNDS {
        let (_order, open_t) = client.market_buy_timed(market_id, min_size_steps).await;

        tokio::time::sleep(nonce_settle).await;

        let close_result = client.close_position_timed(market_id, step_size).await;
        let (close_t, getpos_ms) = match close_result {
            Some((_order, t, gp)) => (t, gp),
            None => panic!("no position to close"),
        };

        println!(
            "  #{} | open: {:6.1}ms | close: {:6.1}ms",
            i + 1, open_t.submit_ms, close_t.submit_ms
        );

        mkt_open_timings.push(open_t);
        mkt_close_timings.push(close_t);

        if i < ROUNDS - 1 { tokio::time::sleep(nonce_settle).await; }
    }




    tokio::time::sleep(nonce_settle).await;

    // ══════════════════════════════════════════════════════════
    // Test 2: Limit post-only (place + cancel)
    // ══════════════════════════════════════════════════════════
    println!("\n== Limit Post-Only Orders: {} rounds ==\n", ROUNDS);

    let mut lmt_place_timings = Vec::new();
    let mut lmt_cancel_timings = Vec::new();

    for i in 0..ROUNDS {
        let ob = client.get_orderbook(market_id, 1).await;
        let best_bid: f64 = ob.bids.first()
            .map(|l| l.price.parse().unwrap())
            .unwrap_or_else(|| market.last_price.parse().unwrap());
        let price_ticks = ((best_bid - 100.0) / step_price).round() as u32;

        let (_order, place_t) = client.limit_buy_timed(market_id, min_size_steps, price_ticks, true).await;

        tokio::time::sleep(nonce_settle).await;

        let (_cancel, cancel_t) = client.cancel_all_timed(market_id).await;

        println!(
            "  #{} | place: {:6.1}ms | cancel: {:6.1}ms",
            i + 1, place_t.submit_ms, cancel_t.submit_ms
        );

        lmt_place_timings.push(place_t);
        lmt_cancel_timings.push(cancel_t);

        if i < ROUNDS - 1 { tokio::time::sleep(nonce_settle).await; }
    }


    print_summary(&[
        compute_stats("mkt buy", &mkt_open_timings),
        compute_stats("mkt sell", &mkt_close_timings),
        compute_stats("lmt place", &lmt_place_timings),
        compute_stats("lmt cancel", &lmt_cancel_timings),
    ]);
}
