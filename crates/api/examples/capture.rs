//! Captures scrubbed raw dispatches: `RXCORD_TOKEN=... cargo run -p rxcord-api --example capture`.
use rxcord_api::{gateway::run_gateway_raw, scrub};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let token = std::env::var("RXCORD_TOKEN").expect("set RXCORD_TOKEN");
    let secs: u64 = std::env::var("RXCORD_CAPTURE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/captured");
    std::fs::create_dir_all(dir).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let task = tokio::spawn(run_gateway_raw(token, tx));
    let end = Instant::now() + Duration::from_secs(secs);
    let mut seq = 0u32;
    while let Ok(Some((name, d))) = tokio::time::timeout_at(end.into(), rx.recv()).await {
        let mut v: serde_json::Value = serde_json::from_str(d.get()).unwrap();
        scrub(&mut v);
        std::fs::write(format!("{dir}/{seq:04}_{name}.json"), serde_json::to_string_pretty(&v).unwrap()).unwrap();
        seq += 1;
    }
    task.abort();
    println!("captured {seq} dispatches into {dir}");
}
