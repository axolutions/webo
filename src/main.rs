use std::sync::Arc;
use tokio::sync::RwLock;
use webo::{collector, metrics::State, server};

#[tokio::main]
async fn main() {
    let bind = std::env::var("WEBO_BIND").unwrap_or_else(|_| "0.0.0.0:5050".into());
    let sample_secs: u64 = std::env::var("WEBO_SAMPLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    // 24 h of history
    let cap = (24 * 3600 / sample_secs.max(1)) as usize;

    let state: server::Shared = Arc::new(RwLock::new(State::new(cap)));
    tokio::spawn(collector::run(state.clone(), sample_secs));

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("webo serving at http://{bind}");
    axum::serve(listener, server::app(state)).await.expect("serve");
}
