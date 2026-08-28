use crate::metrics::State;
use axum::extract::{Query, State as AxumState};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type Shared = Arc<RwLock<State>>;

/// Versioned API: this is the contract the MCP server will consume later —
/// everything the panel shows comes from here, nothing is UI-only.
pub fn app(state: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/history", get(history))
        .route("/api/v1/processes", get(processes))
        .route("/api/v1/system", get(system))
        .with_state(state)
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

async fn processes(AxumState(state): AxumState<Shared>) -> impl IntoResponse {
    let st = state.read().await;
    Json(serde_json::json!({
        "total": st.snapshot.processes,
        "processes": st.processes,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{ProcessGroup, Snapshot};
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn state_with_data() -> Shared {
        let mut st = State::new(10);
        st.system.hostname = "test-host".into();
        st.system.webo_version = "9.9.9".into();
        for i in 0..5u64 {
            st.push(Snapshot {
                ts: 1_000_000 + i * 60,
                cpu_pct: i as f32,
                mem_used: 1024 * i,
                ..Default::default()
            });
        }
        st.snapshot.processes = Some(123);
        st.processes = vec![ProcessGroup {
            pid: 1,
            name: "codo".into(),
            cmd: "/usr/local/bin/codo serve".into(),
            kind: "generic".into(),
            uptime_secs: 60,
            cpu_pct: 1.5,
            mem_bytes: 2048,
            disk_bps: 0,
            procs: 1,
            children: Vec::new(),
        }];
        Arc::new(RwLock::new(st))
    }

    async fn get_json(path: &str) -> (StatusCode, serde_json::Value) {
        let res = app(state_with_data())
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn healthz_answers_ok() {
        let res = app(state_with_data())
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn index_serves_the_panel() {
        let res = app(state_with_data())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1 << 22).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("<!doctype html>"));
        assert!(html.contains("webo"));
    }

    #[tokio::test]
    async fn snapshot_returns_the_latest_sample() {
        let (status, json) = get_json("/api/v1/snapshot").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["cpu_pct"], 4.0);
        assert_eq!(json["processes"], 123);
    }

    #[tokio::test]
    async fn system_returns_machine_identity() {
        let (status, json) = get_json("/api/v1/system").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["hostname"], "test-host");
        assert_eq!(json["webo_version"], "9.9.9");
    }

    #[tokio::test]
    async fn history_returns_all_without_filter() {
        let (status, json) = get_json("/api/v1/history").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json.as_array().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn history_honors_the_minutes_window() {
        // snapshot.ts = 1_000_240; a 2-minute window keeps ts >= 1_000_120
        let (status, json) = get_json("/api/v1/history?minutes=2").await;
        assert_eq!(status, StatusCode::OK);
        let items = json.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|s| s["ts"].as_u64().unwrap() >= 1_000_120));
    }

    #[tokio::test]
    async fn processes_returns_total_and_groups() {
        let (status, json) = get_json("/api/v1/processes").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["total"], 123);
        assert_eq!(json["processes"][0]["name"], "codo");
        assert_eq!(json["processes"][0]["procs"], 1);
    }
}
