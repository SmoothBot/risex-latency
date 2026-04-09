use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const WS_URL: &str = "wss://ws.staging.rise.trade/ws";

#[tokio::main]
async fn main() {
    // Subscribe to all markets to trigger the log_index rollback
    let market_ids: Vec<u32> = (1..=50).collect();
    let subscribe_msg = serde_json::json!({
        "method": "subscribe",
        "params": {
            "channel": "orderbook",
            "market_ids": market_ids
        }
    });

    eprintln!("Connecting to {}...", WS_URL);
    let (ws_stream, _) = connect_async(WS_URL).await.expect("Failed to connect");
    eprintln!("Connected.");

    let (mut write, mut read) = ws_stream.split();

    // Send subscribe message
    write
        .send(Message::Text(subscribe_msg.to_string()))
        .await
        .expect("Failed to send subscribe");
    eprintln!("Sent subscribe message.");

    let mut count = 0u64;
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                count += 1;
                let now = chrono::Utc::now().format("%H:%M:%S%.3f");
                // Try to extract key fields for compact logging
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let block = v.get("block_number");
                    let log_idx = v.get("log_index");
                    let checksum = v.get("checksum");
                    let channel = v.get("channel").and_then(|c| c.as_str()).unwrap_or("?");
                    eprintln!(
                        "[{}] #{} ch={} block={} log_index={} checksum={}",
                        now,
                        count,
                        channel,
                        block.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),
                        log_idx.map(|l| l.to_string()).unwrap_or_else(|| "null".into()),
                        checksum.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
                    );
                    // Print the full JSON to stdout for capture
                    println!("{}", text);
                } else {
                    eprintln!("[{}] #{} (non-json): {}", now, count, text);
                    println!("{}", text);
                }
            }
            Ok(Message::Close(frame)) => {
                eprintln!("Connection closed: {:?}", frame);
                break;
            }
            Ok(Message::Ping(_)) => {}
            Ok(other) => {
                eprintln!("Other message: {:?}", other);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }

    eprintln!("Total messages received: {}", count);
}
