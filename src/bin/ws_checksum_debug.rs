use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const WS_URL: &str = "wss://ws.rise.trade/ws";

#[derive(Debug, Clone, Deserialize)]
struct OrderbookLevel {
    price: String,
    quantity: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OrderedPrice(Decimal);
impl Eq for OrderedPrice {}
impl PartialOrd for OrderedPrice {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OrderedPrice {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.0.cmp(&other.0) }
}

type Book = BTreeMap<OrderedPrice, (String, String)>;

fn to_wei_string(decimal_str: &str) -> String {
    let d = Decimal::from_str(decimal_str).unwrap();
    let wei = d * Decimal::from_str("1000000000000000000").unwrap();
    let truncated = wei.trunc();
    let s = truncated.to_string();
    s.strip_suffix(".0").unwrap_or(&s).to_string()
}

fn compute_checksum(bids: &Book, asks: &Book) -> u32 {
    let bids_desc: Vec<&(String, String)> = bids.values().rev().collect();
    let asks_asc: Vec<&(String, String)> = asks.values().collect();
    let max_len = bids_desc.len().max(asks_asc.len());
    let mut parts: Vec<String> = Vec::new();

    for i in 0..max_len {
        if let Some((price, qty)) = bids_desc.get(i) {
            parts.push(to_wei_string(price));
            parts.push(to_wei_string(qty));
        }
        if let Some((price, qty)) = asks_asc.get(i) {
            parts.push(to_wei_string(price));
            parts.push(to_wei_string(qty));
        }
    }

    let s = parts.join(":");
    crc32fast::hash(s.as_bytes())
}

fn parse_market_id(v: &serde_json::Value) -> Option<u64> {
    v.get("market_id")
        .or(v.get("data").and_then(|d| d.get("market_id")))
        .and_then(|m| match m {
            serde_json::Value::Number(n) => n.as_u64(),
            serde_json::Value::String(s) => s.parse().ok(),
            _ => None,
        })
}

#[tokio::main]
async fn main() {
    println!("=== Wei conversion test ===");
    println!("  69332.4 -> {}", to_wei_string("69332.4"));
    println!("  0.000722 -> {}", to_wei_string("0.000722"));
    println!("  0 -> {}", to_wei_string("0"));
    println!();

    let subscribe_msg = serde_json::json!({
        "method": "subscribe",
        "params": { "channel": "orderbook", "market_ids": [1, 2, 3, 4, 5, 6, 7, 8] }
    });

    let (ws_stream, _) = connect_async(WS_URL).await.expect("Failed to connect");
    let (mut write, mut read) = ws_stream.split();
    write.send(Message::Text(subscribe_msg.to_string())).await.unwrap();

    let mut books: HashMap<u64, (Book, Book)> = HashMap::new();
    let mut count = 0;
    let mut ok_count = 0;
    let mut fail_count = 0;

    while let Some(Ok(Message::Text(text))) = read.next().await {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let method = v.get("type").or(v.get("method")).and_then(|m| m.as_str()).unwrap_or("");
        if method == "subscribe" || method.is_empty() { continue; }

        let market_id = match parse_market_id(&v) { Some(id) => id, None => continue };
        let data = match v.get("data") { Some(d) => d, None => continue };

        let msg_bids: Vec<OrderbookLevel> = data.get("bids")
            .and_then(|b| serde_json::from_value(b.clone()).ok()).unwrap_or_default();
        let msg_asks: Vec<OrderbookLevel> = data.get("asks")
            .and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default();

        let (bids, asks) = books.entry(market_id).or_insert_with(|| (BTreeMap::new(), BTreeMap::new()));

        if method == "snapshot" {
            bids.clear(); asks.clear();
            for l in &msg_bids {
                let p = Decimal::from_str(&l.price).unwrap();
                bids.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone()));
            }
            for l in &msg_asks {
                let p = Decimal::from_str(&l.price).unwrap();
                asks.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone()));
            }
            println!("SNAPSHOT mkt={}: bids={} asks={}", market_id, bids.len(), asks.len());
            continue;
        }

        // Apply update
        for l in &msg_bids {
            let p = Decimal::from_str(&l.price).unwrap();
            if l.quantity == "0" { bids.remove(&OrderedPrice(p)); }
            else { bids.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone())); }
        }
        for l in &msg_asks {
            let p = Decimal::from_str(&l.price).unwrap();
            if l.quantity == "0" { asks.remove(&OrderedPrice(p)); }
            else { asks.insert(OrderedPrice(p), (l.price.clone(), l.quantity.clone())); }
        }

        let expected = v.get("checksum").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
        let computed = compute_checksum(bids, asks);
        count += 1;

        if computed == expected {
            ok_count += 1;
        } else {
            fail_count += 1;
            println!("MISMATCH #{} mkt={} bids={} asks={} expected={} computed={}",
                count, market_id, bids.len(), asks.len(), expected, computed);
        }

        if count % 50 == 0 {
            println!("[{}] checked={} ok={} fail={}",
                chrono::Utc::now().format("%H:%M:%S"), count, ok_count, fail_count);
        }

        if count >= 200 { break; }
    }

    println!("\nFinal: checked={} ok={} fail={}", count, ok_count, fail_count);
}
