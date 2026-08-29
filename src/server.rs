use crate::metrics::State;
use crate::store::Store;
use axum::extract::{Path as AxumPath, Query, State as AxumState};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type Shared = Arc<RwLock<State>>;

/// Everything the handlers need: live state + persistent store.
#[derive(Clone)]
pub struct Api {
    pub state: Shared,
    pub store: Arc<Store>,
}

/// Versioned API: this is the contract the MCP server will consume later —
/// everything the panel shows comes from here, nothing is UI-only.
pub fn app(api: Api) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/history", get(history))
        .route("/api/v1/processes", get(processes))
        .route("/api/v1/system", get(system))
        .route("/api/v1/projects", get(projects_list))
        .route("/api/v1/projects/{slug}", get(project_detail))
        .with_state(api)
}

async fn index() -> impl IntoResponse {
    Html(include_str!("../web/index.html"))
}

async fn snapshot(AxumState(api): AxumState<Api>) -> impl IntoResponse {
    Json(api.state.read().await.snapshot.clone())
}

async fn system(AxumState(api): AxumState<Api>) -> impl IntoResponse {
    Json(api.state.read().await.system.clone())
}

async fn processes(AxumState(api): AxumState<Api>) -> impl IntoResponse {
    let st = api.state.read().await;
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
    AxumState(api): AxumState<Api>,
    Query(q): Query<HistoryQuery>,
) -> impl IntoResponse {
    let st = api.state.read().await;
    let cutoff = q
        .minutes
        .map(|m| st.snapshot.ts.saturating_sub(m * 60))
        .unwrap_or(0);
    let samples: Vec<_> = st.history.iter().filter(|s| s.ts >= cutoff).copied().collect();
    Json(samples)
}

async fn projects_list(AxumState(api): AxumState<Api>) -> impl IntoResponse {
    let rows = api.store.projects().unwrap_or_default();
    let st = api.state.read().await;
    let list: Vec<serde_json::Value> = rows
        .iter()
        .map(|p| {
            let live = st.projects_live.get(&p.slug);
            let last_build = api.store.builds(p.id, 1).ok().and_then(|b| b.into_iter().next());
            let current_version = api
                .store
                .versions(p.id, 20)
                .ok()
                .and_then(|vs| vs.into_iter().find(|v| v.current));
            serde_json::json!({
                "slug": p.slug,
                "name": p.name,
                "source": p.source,
                "repo": p.repo_owner.as_ref().zip(p.repo_name.as_ref())
                    .map(|(o, n)| format!("{o}/{n}")),
                "domain": p.domain,
                "running": live.is_some(),
                "containers": live.map(|l| l.containers.len()).unwrap_or(0),
                "cpu_pct": live.map(|l| l.cpu_pct).unwrap_or(0.0),
                "mem_bytes": live.map(|l| l.mem_bytes).unwrap_or(0),
                "size_bytes": live.map(|l| l.image_bytes + l.volume_bytes).unwrap_or(0),
                "last_build": last_build,
                "current_version": current_version.map(|v| v.tag),
            })
        })
        .collect();
    Json(serde_json::json!({ "projects": list }))
}

async fn project_detail(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "project not found"})))
            .into_response();
    };
    let st = api.state.read().await;
    let live = st.projects_live.get(&p.slug).cloned().unwrap_or_default();
    let builds = api.store.builds(p.id, 10).unwrap_or_default();
    let versions = api.store.versions(p.id, 8).unwrap_or_default();
    Json(serde_json::json!({
        "slug": p.slug,
        "name": p.name,
        "source": p.source,
        "repo_owner": p.repo_owner,
        "repo_name": p.repo_name,
        "domain": p.domain,
        "running": !live.containers.is_empty(),
        "cpu_pct": live.cpu_pct,
        "mem_bytes": live.mem_bytes,
        "disk_bps": live.disk_bps,
        "image_bytes": live.image_bytes,
        "volume_bytes": live.volume_bytes,
        "containers": live.containers,
        "history": live.history,
        "builds": builds,
        "versions": versions,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{ProcessGroup, ProjectContainer, ProjectLive, ProjectSample, Snapshot};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    fn api_with_data() -> Api {
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

        let store = Store::open_in_memory().unwrap();
        store
            .upsert_discovered("codo", "codo", Some(("murichristopher", "codo")), Some("codo.example.com"), 100)
            .unwrap();
        store.upsert_discovered("cloudflared", "cloudflared", None, None, 100).unwrap();
        let id = store.project_by_slug("codo").unwrap().unwrap().id;
        store
            .replace_builds(
                id,
                &[crate::store::Build {
                    run_id: 42,
                    workflow: "Deploy".into(),
                    status: "completed".into(),
                    conclusion: Some("success".into()),
                    commit_sha: "4f44710".into(),
                    commit_msg: "feat: nice".into(),
                    branch: "master".into(),
                    duration_secs: 325,
                    created_at: 999,
                }],
            )
            .unwrap();
        store
            .replace_versions(
                id,
                &[crate::store::Version {
                    tag: "4f44710".into(),
                    current: true,
                    size_bytes: None,
                    created_at: 999,
                }],
            )
            .unwrap();

        let mut live = ProjectLive {
            containers: vec![ProjectContainer {
                name: "codo".into(),
                image: "ghcr.io/murichristopher/codo:latest".into(),
                state: "running".into(),
                uptime_secs: 3600,
                cpu_pct: 0.3,
                mem_bytes: 210_000_000,
                disk_bps: 12_000,
            }],
            cpu_pct: 0.3,
            mem_bytes: 210_000_000,
            disk_bps: 12_000,
            image_bytes: 96_000_000,
            volume_bytes: 2_700_000,
            ..Default::default()
        };
        live.history.push_back(ProjectSample { ts: 1, cpu_pct: 0.3, mem_bytes: 210_000_000 });
        st.projects_live.insert("codo".into(), live);

        Api { state: Arc::new(RwLock::new(st)), store: Arc::new(store) }
    }

    async fn get_json(path: &str) -> (StatusCode, serde_json::Value) {
        let res = app(api_with_data())
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
        let res = app(api_with_data())
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"ok");
    }

    #[tokio::test]
    async fn index_serves_the_panel() {
        let res = app(api_with_data())
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

    #[tokio::test]
    async fn projects_list_merges_store_and_live() {
        let (status, json) = get_json("/api/v1/projects").await;
        assert_eq!(status, StatusCode::OK);
        let list = json["projects"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        let codo = list.iter().find(|p| p["slug"] == "codo").unwrap();
        assert_eq!(codo["repo"], "murichristopher/codo");
        assert_eq!(codo["running"], true);
        assert_eq!(codo["containers"], 1);
        assert_eq!(codo["size_bytes"], 98_700_000);
        assert_eq!(codo["last_build"]["conclusion"], "success");
        assert_eq!(codo["current_version"], "4f44710");
        let cf = list.iter().find(|p| p["slug"] == "cloudflared").unwrap();
        assert_eq!(cf["repo"], serde_json::Value::Null);
        assert_eq!(cf["running"], false);
    }

    #[tokio::test]
    async fn project_detail_returns_everything() {
        let (status, json) = get_json("/api/v1/projects/codo").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["name"], "codo");
        assert_eq!(json["domain"], "codo.example.com");
        assert_eq!(json["containers"][0]["name"], "codo");
        assert_eq!(json["history"].as_array().unwrap().len(), 1);
        assert_eq!(json["builds"][0]["commit_sha"], "4f44710");
        assert_eq!(json["versions"][0]["current"], true);
        assert_eq!(json["image_bytes"], 96_000_000);
    }

    #[tokio::test]
    async fn project_detail_404_for_unknown_slug() {
        let (status, json) = get_json("/api/v1/projects/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["error"], "project not found");
    }
}
