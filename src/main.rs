use std::sync::Arc;
use tokio::sync::RwLock;
use webo::{collector, github, metrics::State, projects, server, store::Store};

#[tokio::main]
async fn main() {
    let bind = std::env::var("WEBO_BIND").unwrap_or_else(|_| "0.0.0.0:5050".into());
    let sample_secs: u64 = std::env::var("WEBO_SAMPLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    // 24 h of history
    let cap = (24 * 3600 / sample_secs.max(1)) as usize;

    let db_path = std::env::var("WEBO_DB_PATH").unwrap_or_else(|_| "webo.db".into());
    let store = Arc::new(Store::open(std::path::Path::new(&db_path)).expect("open store"));

    let state: server::Shared = Arc::new(RwLock::new(State::new(cap)));
    tokio::spawn(collector::run(state.clone(), sample_secs));
    tokio::spawn(projects::run(state.clone(), store.clone(), sample_secs));
    tokio::spawn(github::run(store.clone(), 120));

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("webo serving at http://{bind}");
    axum::serve(listener, server::app(server::Api { state, store })).await.expect("serve");
}
