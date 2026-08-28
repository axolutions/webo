mod collector;
mod metrics;

use axum::extract::{Query, State as AxumState};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use metrics::State;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

type Shared = Arc<RwLock<State>>;

#[tokio::main]
async fn main() {
    let bind = std::env::var("WEBO_BIND").unwrap_or_else(|_| "0.0.0.0:5050".into());
    let sample_secs: u64 = std::env::var("WEBO_SAMPLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    // 24 h of history
    let cap = (24 * 3600 / sample_secs.max(1)) as usize;

    let state: Shared = Arc::new(RwLock::new(State::new(cap)));
    tokio::spawn(collector::run(state.clone(), sample_secs));

    // Versioned API: this is the contract the MCP server will consume later —
    // everything the panel shows comes from here, nothing is UI-only.
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/history", get(history))
        .route("/api/v1/system", get(system))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("webo servindo em http://{bind}");
    axum::serve(listener, app).await.expect("serve");
}

async fn index() -> impl IntoResponse {
    Html(include_str!("../web/index.html"))
}

async fn snapshot(AxumState(state): AxumState<Shared>) -> impl IntoResponse {
    Json(state.read().await.snapshot.clone())
}

async fn system(AxumState(state): AxumState<Shared>) -> impl IntoResponse {
    Json(state.read().await.system.clone())
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// window in minutes (default: 24 h)
    minutes: Option<u64>,
}

async fn history(
    AxumState(state): AxumState<Shared>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    let st = state.read().await;
    let cutoff = q
        .minutes
        .map(|m| st.snapshot.ts.saturating_sub(m * 60))
        .unwrap_or(0);
    let samples: Vec<_> = st.history.iter().filter(|s| s.ts >= cutoff).copied().collect();
    Json(samples)
}
