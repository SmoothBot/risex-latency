use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const WS_URL: &str = "wss://ws.staging.rise.trade/ws";
const LOG_FILE: &str = "/tmp/ws_logindex_monitor_staging.log";

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct WsMessage {
    method: Option<String>,
    channel: Option<String>,
    market_id: Option<serde_json::Value>,
    block_number: Option<u64>,
    log_index: Option<u64>,
    #[serde(rename = "type")]
    msg_type: Option<String>,
    level_count: Option<u64>,
    worker_timestamp: Option<String>,
    data: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct MarketState {
    block_number: u64,
    log_index: u64,
    msg_count: u64,
    last_method: String,
    last_raw: String,
}

fn parse_market_id(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

macro_rules! log {
    ($f:expr, $($arg:tt)*) => {{
        let line = format!($($arg)*);
        let _ = writeln!($f, "{}", line);
        let _ = $f.flush();
        eprintln!("{}", line);
    }};
}

#[tokio::main]
async fn main() {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(LOG_FILE)
        .expect("failed to open log file");

    log!(f, "=== ws_logindex_monitor started ===");
    log!(f, "Log file: {}", LOG_FILE);

    let market_ids: Vec<u32> = (1..=50).collect();
    let subscribe_msg = serde_json::json!({
        "method": "subscribe",
        "params": {
            "channel": "orderbook",
            "market_ids": market_ids
        }
    });

    log!(f, "Connecting to {}...", WS_URL);
    let (ws_stream, _) = connect_async(WS_URL).await.expect("Failed to connect");
    log!(f, "Connected. Subscribing to {} markets.", market_ids.len());

    let (mut write, mut read) = ws_stream.split();
    write
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .expect("Failed to send subscribe");
    log!(f, "Subscribed. Monitoring for log_index rollbacks/gaps...");
    log!(f, "");

    let mut states: HashMap<u64, MarketState> = HashMap::new();
    let mut msg_seq: u64 = 0;
    let mut snapshot_count: u64 = 0;
    let mut update_count: u64 = 0;

    while let Some(msg) = read.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(frame)) => {
                log!(f, "Connection closed: {:?}", frame);
                break;
            }
            Ok(Message::Ping(_)) => continue,
            Ok(_) => continue,
            Err(e) => {
                log!(f, "WebSocket error: {}", e);
                break;
            }
        };

        msg_seq += 1;
        let now = chrono::Utc::now().format("%H:%M:%S%.3f");

        let parsed: WsMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                log!(f, "[{}] #{} PARSE ERROR: {} | raw: {}", now, msg_seq, e, &text[..text.len().min(200)]);
                continue;
            }
        };

        // Skip subscribe confirmations
        if parsed.method.as_deref() == Some("subscribe") {
            log!(f, "[{}] #{} subscribe confirmation", now, msg_seq);
            continue;
        }

        let method = parsed.method.as_deref().unwrap_or("unknown");
        let market_id = parsed.market_id.as_ref()
            .or_else(|| parsed.data.as_ref().and_then(|d| d.get("market_id")))
            .and_then(parse_market_id);

        let market_id = match market_id {
            Some(id) => id,
            None => {
                log!(f, "[{}] #{} no market_id: {}", now, msg_seq, &text[..text.len().min(200)]);
                continue;
            }
        };

        let (block_number, log_index) = match (parsed.block_number, parsed.log_index) {
            (Some(b), Some(l)) => (b, l),
            _ => {
                log!(f, "[{}] #{} mkt={} method={} missing block/log_index", now, msg_seq, market_id, method);
                continue;
            }
        };

        let is_snapshot = method == "snapshot";
        if is_snapshot {
            snapshot_count += 1;
            log!(f, "[{}] #{} SNAPSHOT mkt={:<3} block={} log_index={}",
                now, msg_seq, market_id, block_number, log_index);
        } else {
            update_count += 1;
            log!(f, "[{}] #{} UPDATE  mkt={:<3} block={} log_index={} (total updates: {})",
                now, msg_seq, market_id, block_number, log_index, update_count);
        }

        // Check against previous state for this market
        if let Some(prev) = states.get(&market_id) {
            if !is_snapshot {
                let rollback = block_number < prev.block_number
                    || (block_number == prev.block_number && log_index < prev.log_index);

                if rollback {
                    let kind = "ROLLBACK";

                    log!(f, "");
                    log!(f, "{}", "!".repeat(80));
                    log!(f, "  {} DETECTED on market_id={}", kind, market_id);
                    log!(f, "  msg #{}", msg_seq);
                    log!(f, "  prev: block={} log_index={} method={}", prev.block_number, prev.log_index, prev.last_method);
                    log!(f, "  curr: block={} log_index={} method={}", block_number, log_index, method);
                    log!(f, "{}", "!".repeat(80));

                    log!(f, "");
                    log!(f, "--- Previous payload (market {}) ---", market_id);
                    log!(f, "{}", prev.last_raw);
                    log!(f, "");
                    log!(f, "--- Current payload (triggered anomaly) ---");
                    log!(f, "{}", text);

                    log!(f, "");
                    log!(f, "--- All market states at time of anomaly ---");
                    let mut sorted: Vec<_> = states.iter().collect();
                    sorted.sort_by_key(|(id, _)| *id);
                    for (id, st) in &sorted {
                        let marker = if **id == market_id { " <-- ANOMALY" } else { "" };
                        log!(f, "  mkt={:<3} block={} log_index={:<5} msgs={} last_method={}{}",
                            id, st.block_number, st.log_index, st.msg_count, st.last_method, marker);
                    }

                    log!(f, "");
                    log!(f, "--- Stats ---");
                    log!(f, "  total messages: {}", msg_seq);
                    log!(f, "  snapshots: {}", snapshot_count);
                    log!(f, "  updates: {}", update_count);
                    log!(f, "  markets tracked: {}", states.len());

                    // Write structured debug JSON
                    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
                    let debug_path = format!("/tmp/ws_anomaly_staging_{}.json", ts);
                    let debug = serde_json::json!({
                        "anomaly": {
                            "kind": kind,
                            "market_id": market_id,
                            "prev_block": prev.block_number,
                            "prev_log_index": prev.log_index,
                            "new_block": block_number,
                            "new_log_index": log_index,
                            "msg_seq": msg_seq,
                        },
                        "prev_payload": serde_json::from_str::<serde_json::Value>(&prev.last_raw).ok(),
                        "current_payload": serde_json::from_str::<serde_json::Value>(&text).ok(),
                        "market_states": sorted.iter().map(|(id, st)| {
                            serde_json::json!({
                                "market_id": id,
                                "block_number": st.block_number,
                                "log_index": st.log_index,
                                "msg_count": st.msg_count,
                                "last_method": st.last_method,
                            })
                        }).collect::<Vec<_>>(),
                        "stats": {
                            "total_messages": msg_seq,
                            "snapshots": snapshot_count,
                            "updates": update_count,
                        }
                    });
                    std::fs::write(&debug_path, serde_json::to_string_pretty(&debug).unwrap())
                        .expect("failed to write debug file");
                    log!(f, "");
                    log!(f, "Debug JSON written to {}", debug_path);
                    log!(f, "Halting.");
                    return;
                }
            }
        }

        // Update state
        let entry = states.entry(market_id).or_insert(MarketState {
            block_number: 0,
            log_index: 0,
            msg_count: 0,
            last_method: String::new(),
            last_raw: String::new(),
        });
        entry.block_number = block_number;
        entry.log_index = log_index;
        entry.msg_count += 1;
        entry.last_method = method.to_string();
        entry.last_raw = text;
    }

    log!(f, "");
    log!(f, "Connection ended without detecting anomaly.");
    log!(f, "Total: {} messages ({} snapshots, {} updates)", msg_seq, snapshot_count, update_count);
}
