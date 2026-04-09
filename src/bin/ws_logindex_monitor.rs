use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const WS_URL: &str = "wss://ws.rise.trade/ws";
const LOG_FILE: &str = "/tmp/ws_logindex_monitor.log";
const NUM_CONNECTIONS: usize = 10;
const RING_BUFFER_SIZE: usize = 200;
const WEI: &str = "1000000000000000000";

fn concat_block_and_log_index(block_number: u64, log_index: u64) -> u128 {
    u128::from(block_number) << 64 | u128::from(log_index)
}

fn to_wei_string(decimal_str: &str) -> String {
    let d = Decimal::from_str(decimal_str).unwrap_or_default();
    let wei = d * Decimal::from_str(WEI).unwrap();
    let s = wei.trunc().to_string();
    s.strip_suffix(".0").unwrap_or(&s).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OrderedPrice(Decimal);
impl PartialOrd for OrderedPrice {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OrderedPrice {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.0.cmp(&other.0) }
}

type Book = BTreeMap<OrderedPrice, (String, String)>;

fn compute_checksum(bids: &Book, asks: &Book) -> u32 {
    let bids_desc: Vec<&(String, String)> = bids.values().rev().collect();
    let asks_asc: Vec<&(String, String)> = asks.values().collect();
    let max_len = bids_desc.len().max(asks_asc.len());
    let mut parts: Vec<String> = Vec::new();
    for i in 0..max_len {
        if let Some((p, q)) = bids_desc.get(i) {
            parts.push(to_wei_string(p));
            parts.push(to_wei_string(q));
        }
        if let Some((p, q)) = asks_asc.get(i) {
            parts.push(to_wei_string(p));
            parts.push(to_wei_string(q));
        }
    }
    let s = parts.join(":");
    crc32fast::hash(s.as_bytes())
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct WsMessage {
    method: Option<String>,
    channel: Option<String>,
    market_id: Option<serde_json::Value>,
    block_number: Option<u64>,
    log_index: Option<u64>,
    checksum: Option<u32>,
    #[serde(rename = "type")]
    msg_type: Option<String>,
    level_count: Option<u64>,
    worker_timestamp: Option<String>,
    data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct OrderbookLevel {
    price: String,
    quantity: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct RecentMsg {
    conn_id: usize,
    market_id: u64,
    block_number: u64,
    log_index: u64,
    seq: String,
    checksum: Option<u32>,
    deduped: bool,
}

struct MarketStream {
    last_seq: u128,
    seen: HashSet<u128>,
    seen_queue: VecDeque<u128>,
    total_received: u64,
    total_deduped: u64,
    total_duplicates: u64,
}

impl MarketStream {
    fn new() -> Self {
        Self {
            last_seq: 0,
            seen: HashSet::new(),
            seen_queue: VecDeque::new(),
            total_received: 0,
            total_deduped: 0,
            total_duplicates: 0,
        }
    }

    fn process(&mut self, seq: u128) -> (bool, bool) {
        self.total_received += 1;
        if self.seen.contains(&seq) {
            self.total_duplicates += 1;
            return (false, false);
        }
        self.seen.insert(seq);
        self.seen_queue.push_back(seq);
        if self.seen_queue.len() > 10000 {
            if let Some(old) = self.seen_queue.pop_front() { self.seen.remove(&old); }
        }
        self.total_deduped += 1;
        let rollback = self.last_seq > 0 && seq < self.last_seq;
        self.last_seq = seq;
        (true, rollback)
    }
}

struct SharedState {
    file: std::fs::File,
    streams: HashMap<u64, MarketStream>,
    /// Per-connection per-market orderbooks for checksum validation
    /// Only conn 0 maintains books (one connection is enough to validate)
    books: HashMap<u64, (Book, Book)>,
    recent: VecDeque<RecentMsg>,
    msg_seq: u64,
    snapshot_count: u64,
    update_count: u64,
    checksum_ok: u64,
    checksum_fail: u64,
    halted: bool,
}

fn parse_market_id(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_levels(data: &serde_json::Value, key: &str) -> Vec<OrderbookLevel> {
    data.get(key)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

macro_rules! log {
    ($f:expr, $($arg:tt)*) => {{
        use std::io::Write;
        let line = format!($($arg)*);
        let _ = writeln!($f, "{}", line);
        let _ = $f.flush();
        eprintln!("{}", line);
    }};
}

async fn run_connection(conn_id: usize, shared: Arc<Mutex<SharedState>>) {
    let market_ids: Vec<u32> = (1..=50).collect();
    let subscribe_msg = serde_json::json!({
        "method": "subscribe",
        "params": {
            "channel": "orderbook",
            "market_ids": market_ids
        }
    });

    loop {
        let (ws_stream, _) = match connect_async(WS_URL).await {
            Ok(s) => s,
            Err(e) => {
                let mut s = shared.lock().await;
                log!(s.file, "[conn={}] Failed to connect: {}, retrying in 2s...", conn_id, e);
                drop(s);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };

        let (mut write, mut read) = ws_stream.split();
        if let Err(e) = write.send(Message::Text(subscribe_msg.to_string())).await {
            let mut s = shared.lock().await;
            log!(s.file, "[conn={}] Failed to subscribe: {}, retrying in 2s...", conn_id, e);
            drop(s);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }

        {
            let mut s = shared.lock().await;
            log!(s.file, "[conn={}] Connected and subscribed.", conn_id);
        }

        while let Some(msg) = read.next().await {
            let text = match msg {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(frame)) => {
                    let mut s = shared.lock().await;
                    log!(s.file, "[conn={}] Connection closed: {:?}, reconnecting...", conn_id, frame);
                    break;
                }
                Ok(Message::Ping(_)) => continue,
                Ok(_) => continue,
                Err(e) => {
                    let mut s = shared.lock().await;
                    log!(s.file, "[conn={}] WebSocket error: {}, reconnecting...", conn_id, e);
                    break;
                }
            };

            let mut s = shared.lock().await;
            if s.halted { return; }

            s.msg_seq += 1;
            let msg_seq = s.msg_seq;

            let parsed: WsMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(_) => continue,
            };

            if parsed.method.as_deref() == Some("subscribe") { continue; }

            let method = parsed.method.as_deref()
                .or(parsed.msg_type.as_deref())
                .unwrap_or("unknown");
            let market_id = parsed.market_id.as_ref()
                .or_else(|| parsed.data.as_ref().and_then(|d| d.get("market_id")))
                .and_then(parse_market_id);
            let market_id = match market_id { Some(id) => id, None => continue };

            let (block_number, log_index) = match (parsed.block_number, parsed.log_index) {
                (Some(b), Some(l)) => (b, l),
                _ => continue,
            };

            let is_snapshot = method == "snapshot";
            let seq = concat_block_and_log_index(block_number, log_index);

            // === Checksum validation on conn 0 only ===
            if conn_id == 0 {
                if let Some(data) = &parsed.data {
                    let msg_bids = parse_levels(data, "bids");
                    let msg_asks = parse_levels(data, "asks");

                    let checksum_result: Option<(bool, u32, u32, usize, usize)> = {
                        let (bids, asks) = s.books.entry(market_id)
                            .or_insert_with(|| (BTreeMap::new(), BTreeMap::new()));

                        if is_snapshot {
                            bids.clear(); asks.clear();
                            for l in &msg_bids {
                                if let Ok(p) = Decimal::from_str(&l.price) {
                                    bids.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone()));
                                }
                            }
                            for l in &msg_asks {
                                if let Ok(p) = Decimal::from_str(&l.price) {
                                    asks.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone()));
                                }
                            }
                            None
                        } else {
                            for l in &msg_bids {
                                if let Ok(p) = Decimal::from_str(&l.price) {
                                    if l.quantity == "0" { bids.remove(&OrderedPrice(p)); }
                                    else { bids.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone())); }
                                }
                            }
                            for l in &msg_asks {
                                if let Ok(p) = Decimal::from_str(&l.price) {
                                    if l.quantity == "0" { asks.remove(&OrderedPrice(p)); }
                                    else { asks.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone())); }
                                }
                            }
                            parsed.checksum.map(|expected| {
                                let computed = compute_checksum(bids, asks);
                                (computed == expected, expected, computed, bids.len(), asks.len())
                            })
                        }
                    };

                    if let Some((matched, expected, computed, nb, na)) = checksum_result {
                        if matched {
                            s.checksum_ok += 1;
                        } else {
                            s.checksum_fail += 1;
                            let now = chrono::Utc::now().format("%H:%M:%S%.3f");
                            log!(s.file, "[{}] CHECKSUM MISMATCH mkt={} block={} li={} expected={} computed={} bids={} asks={}",
                                now, market_id, block_number, log_index, expected, computed, nb, na);
                        }
                    }
                }
            }

            if is_snapshot {
                s.snapshot_count += 1;
                continue;
            }

            s.update_count += 1;

            // Feed into per-market dedup stream
            let (is_new, is_rollback) = {
                let stream = s.streams.entry(market_id).or_insert_with(MarketStream::new);
                stream.process(seq)
            };

            // Store in ring buffer
            s.recent.push_back(RecentMsg {
                conn_id, market_id, block_number, log_index,
                seq: seq.to_string(), checksum: parsed.checksum, deduped: is_new,
            });
            if s.recent.len() > RING_BUFFER_SIZE { s.recent.pop_front(); }

            // Progress counter every 100 deduped messages
            if is_new {
                let total_deduped: u64 = s.streams.values().map(|st| st.total_deduped).sum();
                if total_deduped % 100 == 0 {
                    let now = chrono::Utc::now().format("%H:%M:%S%.3f");
                    let total_dupes: u64 = s.streams.values().map(|st| st.total_duplicates).sum();
                    let pid = std::process::id();
                    let cpu = std::fs::read_to_string(format!("/proc/{}/stat", pid))
                        .ok()
                        .and_then(|stat| {
                            let fields: Vec<&str> = stat.split_whitespace().collect();
                            let utime: u64 = fields.get(13)?.parse().ok()?;
                            let stime: u64 = fields.get(14)?.parse().ok()?;
                            Some(utime + stime)
                        })
                        .unwrap_or(0);
                    let rss = std::fs::read_to_string(format!("/proc/{}/statm", pid))
                        .ok()
                        .and_then(|statm| {
                            let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
                            Some(pages * 4096 / 1024 / 1024)
                        })
                        .unwrap_or(0);
                    log!(s.file, "[{}] raw={} deduped={} dupes={} cksum_ok={} cksum_fail={} cpu={} rss={}MB",
                        now, s.update_count, total_deduped, total_dupes,
                        s.checksum_ok, s.checksum_fail, cpu, rss);
                }
            }

            if is_rollback {
                s.halted = true;
                let now = chrono::Utc::now().format("%H:%M:%S%.3f");

                let prev_deduped: Option<RecentMsg> = s.recent.iter().rev()
                    .filter(|m| m.market_id == market_id && m.deduped)
                    .nth(1).cloned();

                log!(s.file, "");
                log!(s.file, "{}", "!".repeat(80));
                log!(s.file, "  SEQ ROLLBACK on mkt={} (deduped stream)", market_id);
                log!(s.file, "  [{}] raw msg #{}", now, msg_seq);
                if let Some(prev) = &prev_deduped {
                    log!(s.file, "  prev: block={} li={} seq={} (from conn={})",
                        prev.block_number, prev.log_index, prev.seq, prev.conn_id);
                }
                log!(s.file, "  curr: block={} li={} seq={} (from conn={})",
                    block_number, log_index, seq, conn_id);
                log!(s.file, "{}", "!".repeat(80));

                let mut market_stats: Vec<_> = s.streams.iter()
                    .map(|(id, st)| (*id, st.last_seq, st.total_received, st.total_deduped, st.total_duplicates))
                    .collect();
                market_stats.sort_by_key(|(id, _, _, _, _)| *id);
                let ring_copy: Vec<RecentMsg> = s.recent.iter().cloned().collect();

                log!(s.file, "");
                log!(s.file, "--- Recent {} messages ---", ring_copy.len());
                for msg in &ring_copy {
                    let tag = if msg.deduped { "NEW " } else { "DUP " };
                    let marker = if msg.market_id == market_id { " <--" } else { "" };
                    log!(s.file, "  {} c={} mkt={:<3} block={} li={:<5} cksum={:?}{}",
                        tag, msg.conn_id, msg.market_id, msg.block_number, msg.log_index, msg.checksum, marker);
                }

                log!(s.file, "");
                log!(s.file, "--- Stats ---");
                log!(s.file, "  raw={} deduped={} dupes={} cksum_ok={} cksum_fail={}",
                    s.update_count,
                    market_stats.iter().map(|s| s.3).sum::<u64>(),
                    market_stats.iter().map(|s| s.4).sum::<u64>(),
                    s.checksum_ok, s.checksum_fail);

                let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                let debug_path = format!("/tmp/ws_anomaly_{}.json", ts);
                let debug = serde_json::json!({
                    "anomaly": { "kind": "PER_MARKET_SEQ_ROLLBACK", "market_id": market_id,
                        "conn_id": conn_id, "block": block_number, "li": log_index, "seq": seq.to_string(), "msg_seq": msg_seq },
                    "recent_messages": ring_copy,
                    "stats": { "raw": s.update_count, "cksum_ok": s.checksum_ok, "cksum_fail": s.checksum_fail }
                });
                std::fs::write(&debug_path, serde_json::to_string_pretty(&debug).unwrap()).ok();
                log!(s.file, "Debug JSON written to {}", debug_path);
                log!(s.file, "Halting.");
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[tokio::main]
async fn main() {
    let file = std::fs::OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(LOG_FILE).expect("failed to open log file");

    let shared = Arc::new(Mutex::new(SharedState {
        file,
        streams: HashMap::new(),
        books: HashMap::new(),
        recent: VecDeque::with_capacity(RING_BUFFER_SIZE + 1),
        msg_seq: 0,
        snapshot_count: 0,
        update_count: 0,
        checksum_ok: 0,
        checksum_fail: 0,
        halted: false,
    }));

    {
        let mut s = shared.lock().await;
        log!(s.file, "=== ws_logindex_monitor started ===");
        log!(s.file, "{} conns | dedup by block<<64|li | seq check | checksum validation (wei CRC32)", NUM_CONNECTIONS);
        log!(s.file, "");
    }

    let mut handles = Vec::new();
    for conn_id in 0..NUM_CONNECTIONS {
        let shared = shared.clone();
        handles.push(tokio::spawn(run_connection(conn_id, shared)));
    }

    futures_util::future::select_all(handles).await;

    let s = shared.lock().await;
    if !s.halted {
        eprintln!("\nEnded without rollback. raw={} cksum_ok={} cksum_fail={}", s.update_count, s.checksum_ok, s.checksum_fail);
    }
}
