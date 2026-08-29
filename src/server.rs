use crate::metrics::State;
use crate::store::Store;
use crate::{github, projects, scaffold};
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
        .route("/api/v1/projects", get(projects_list).post(project_create))
        .route("/api/v1/projects/{slug}", get(project_detail))
        .route("/api/v1/projects/{slug}/provision", axum::routing::post(project_provision))
        .route("/api/v1/github/repos", get(github_repos))
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
                "tech": p.tech,
                "status": p.status,
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
        "tech": p.tech,
        "status": p.status,
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

fn github_token() -> Option<String> {
    std::env::var("WEBO_GITHUB_TOKEN").ok().filter(|t| !t.trim().is_empty())
}

fn err(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

async fn github_repos(AxumState(api): AxumState<Api>) -> impl IntoResponse {
    let Some(token) = github_token() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "github token not configured");
    };
    let repos = tokio::task::spawn_blocking(move || github::list_repos(&token))
        .await
        .unwrap_or_default();
    let taken: std::collections::HashSet<(String, String)> = api
        .store
        .projects()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.repo_owner.zip(p.repo_name))
        .collect();
    let list: Vec<serde_json::Value> = repos
        .into_iter()
        .map(|r| {
            let already = taken.contains(&(r.owner.clone(), r.name.clone()));
            serde_json::json!({
                "owner": r.owner,
                "name": r.name,
                "private": r.private,
                "language": r.language,
                "pushed_at": r.pushed_at,
                "default_branch": r.default_branch,
                "already_project": already,
            })
        })
        .collect();
    Json(serde_json::json!({ "repos": list })).into_response()
}

#[derive(Deserialize)]
struct CreateProject {
    repo_owner: String,
    repo_name: String,
}

fn valid_repo_part(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Everything create/provision need to know about the repo, in one blocking hop.
struct RepoScan {
    branch: String,
    language: Option<String>,
    template: Option<scaffold::Template>,
    has_dockerfile: bool,
}

fn scan_repo(token: &str, owner: &str, name: &str) -> Option<RepoScan> {
    let info = github::repo_info(token, owner, name)?;
    let gemfile = github::get_file(token, owner, name, "Gemfile");
    let package_json = github::get_file(token, owner, name, "package.json");
    let has_dockerfile = github::get_file(token, owner, name, "Dockerfile").is_some();
    Some(RepoScan {
        branch: info.default_branch,
        language: info.language,
        template: scaffold::detect(gemfile.as_deref(), package_json.as_deref()),
        has_dockerfile,
    })
}

fn secrets_json() -> Vec<serde_json::Value> {
    scaffold::SECRET_NAMES
        .iter()
        .map(|n| {
            serde_json::json!({ "name": n, "configured": std::env::var(n).is_ok_and(|v| !v.is_empty()) })
        })
        .collect()
}

async fn project_create(
    AxumState(api): AxumState<Api>,
    Json(req): Json<CreateProject>,
) -> impl IntoResponse {
    let Some(token) = github_token() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "github token not configured");
    };
    if !valid_repo_part(&req.repo_owner) || !valid_repo_part(&req.repo_name) {
        return err(StatusCode::BAD_REQUEST, "invalid repository");
    }
    let (owner, name) = (req.repo_owner.clone(), req.repo_name.clone());
    let scan = tokio::task::spawn_blocking({
        let (token, owner, name) = (token.clone(), owner.clone(), name.clone());
        move || scan_repo(&token, &owner, &name)
    })
    .await
    .ok()
    .flatten();
    let Some(scan) = scan else {
        return err(StatusCode::NOT_FOUND, "repository not found or unreadable");
    };
    let Some(template) = scan.template else {
        return Json(serde_json::json!({
            "supported": false,
            "language": scan.language,
        }))
        .into_response();
    };
    let tech = match template {
        scaffold::Template::Rails => "ruby",
        scaffold::Template::Next => "next",
    };
    let slug = projects::slug_for(&name);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if api.store.register(&slug, &owner, &name, tech, now).is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "could not register the project");
    }
    let files = scaffold::plan(template, &slug, &owner, &name, &scan.branch, scan.has_dockerfile);
    Json(serde_json::json!({
        "supported": true,
        "slug": slug,
        "template": template,
        "tech": tech,
        "branch": scan.branch,
        "has_dockerfile": scan.has_dockerfile,
        "files": files.iter().map(|f| &f.path).collect::<Vec<_>>(),
        "secrets": secrets_json(),
    }))
    .into_response()
}

async fn project_provision(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
) -> impl IntoResponse {
    let Some(token) = github_token() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "github token not configured");
    };
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let (Some(owner), Some(name)) = (p.repo_owner.clone(), p.repo_name.clone()) else {
        return err(StatusCode::BAD_REQUEST, "project has no repository connected");
    };

    let result = tokio::task::spawn_blocking({
        let (token, owner, name, slug) = (token.clone(), owner.clone(), name.clone(), slug.clone());
        move || {
            let scan = scan_repo(&token, &owner, &name).ok_or("repository unreadable")?;
            let template = scan.template.ok_or("technology not supported")?;
            let files = scaffold::plan(template, &slug, &owner, &name, &scan.branch, scan.has_dockerfile);
            let label = match template {
                scaffold::Template::Rails => "rails",
                scaffold::Template::Next => "next",
            };
            let sha = github::commit_files(
                &token, &owner, &name, &scan.branch, &files,
                &format!("chore: webo scaffold ({label})"),
            )?;
            let secrets: Vec<serde_json::Value> = scaffold::SECRET_NAMES
                .iter()
                .map(|sname| {
                    let status = match std::env::var(sname) {
                        Ok(v) if !v.is_empty() => match github::set_secret(&token, &owner, &name, sname, &v) {
                            Ok(()) => "created",
                            Err(_) => "failed",
                        },
                        _ => "skipped",
                    };
                    serde_json::json!({ "name": sname, "status": status })
                })
                .collect();
            Ok::<_, String>((sha, files, secrets))
        }
    })
    .await;

    match result {
        Ok(Ok((sha, files, secrets))) => {
            let _ = api.store.set_status(&slug, Some("deploying"));
            tokio::spawn(github::watch_first_deploy(
                api.store.clone(),
                slug.clone(),
                owner,
                name,
            ));
            Json(serde_json::json!({
                "commit_sha": sha,
                "files": files.iter().map(|f| &f.path).collect::<Vec<_>>(),
                "secrets": secrets,
            }))
            .into_response()
        }
        Ok(Err(msg)) => err(StatusCode::BAD_GATEWAY, &msg),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "provision task failed"),
    }
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
        store.set_tech_if_empty("codo", "rust").unwrap();
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
        assert_eq!(codo["tech"], "rust");
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
        assert_eq!(json["tech"], "rust");
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

    #[tokio::test]
    async fn wizard_endpoints_need_a_github_token() {
        let _env = crate::testutil::env_lock();
        std::env::remove_var("WEBO_GITHUB_TOKEN");
        let (status, json) = get_json("/api/v1/github/repos").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(json["error"], "github token not configured");
    }

    async fn post_json(api: Api, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let res = app(api)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    fn b64(text: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    /// A fake GitHub covering the whole wizard: listing, contents, the Git
    /// Data chain, and Actions secrets.
    async fn mock_github() -> String {
        use axum::routing::{get as axget, post as axpost, put as axput};
        use serde_json::json;
        let sk = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
        let pk_b64 = b64_bytes(sk.public_key().to_bytes());
        let router = axum::Router::new()
            .route("/user/repos", axget(|| async {
                axum::Json(json!([
                    {"name": "axofin", "owner": {"login": "muri"}, "private": true,
                     "language": "Ruby", "pushed_at": "2026-08-29T01:00:00Z", "default_branch": "main"},
                    {"name": "codo", "owner": {"login": "murichristopher"}, "private": true,
                     "language": "Rust", "pushed_at": "2026-08-28T01:00:00Z", "default_branch": "master"}
                ]))
            }))
            .route("/repos/muri/axofin", axget(|| async {
                axum::Json(json!({"private": true, "language": "Ruby", "default_branch": "main"}))
            }))
            .route("/repos/muri/axofin/contents/Gemfile", axget(|| async {
                axum::Json(json!({"content": b64("gem \"rails\", \"~> 8.0\"\n")}))
            }))
            .route("/repos/muri/notas/", axget(|| async { axum::Json(json!({})) }))
            .route("/repos/muri/notas", axget(|| async {
                axum::Json(json!({"private": true, "language": "Python", "default_branch": "main"}))
            }))
            .route("/repos/muri/axofin/git/ref/heads/main", axget(|| async {
                axum::Json(json!({"object": {"sha": "headsha"}}))
            }))
            .route("/repos/muri/axofin/git/commits/headsha", axget(|| async {
                axum::Json(json!({"tree": {"sha": "treesha"}}))
            }))
            .route("/repos/muri/axofin/git/trees", axpost(|body: String| async move {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["base_tree"], "treesha");
                assert!(v["tree"].as_array().unwrap().len() >= 3);
                axum::Json(json!({"sha": "newtree"}))
            }))
            .route("/repos/muri/axofin/git/commits", axpost(|body: String| async move {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["parents"][0], "headsha");
                axum::Json(json!({"sha": "a3f9e21fff"}))
            }))
            .route("/repos/muri/axofin/git/refs/heads/main", axum::routing::patch(|| async {
                axum::Json(json!({"ref": "refs/heads/main"}))
            }))
            .route("/repos/muri/axofin/actions/secrets/public-key", axget(move || {
                let pk = pk_b64.clone();
                async move { axum::Json(json!({"key_id": "k1", "key": pk})) }
            }))
            .route("/repos/muri/axofin/actions/secrets/{name}", axput(|| async {
                axum::http::StatusCode::NO_CONTENT
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn b64_bytes(bytes: [u8; 32]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wizard_flow_repos_create_and_provision() {
        let _env = crate::testutil::env_lock();
        let base = mock_github().await;
        std::env::set_var("WEBO_GITHUB_API_BASE", &base);
        std::env::set_var("WEBO_GITHUB_TOKEN", "test-token");
        std::env::set_var("WEBO_DEPLOY_TOKEN", "deploy-secret");
        std::env::remove_var("TS_OAUTH_CLIENT_ID");
        std::env::remove_var("TS_OAUTH_SECRET");

        // repos: codo is already a project, axofin is free
        let api = api_with_data();
        let (status, json) = {
            let res = app(api.clone())
                .oneshot(Request::builder().uri("/api/v1/github/repos").body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = res.status();
            let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
            (status, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap())
        };
        assert_eq!(status, StatusCode::OK);
        let repos = json["repos"].as_array().unwrap();
        let axofin = repos.iter().find(|r| r["name"] == "axofin").unwrap();
        assert_eq!(axofin["already_project"], false);
        let codo = repos.iter().find(|r| r["name"] == "codo").unwrap();
        assert_eq!(codo["already_project"], true);

        // create: rails detected, plan with 4 files (no Dockerfile in the repo)
        let (status, json) = post_json(
            api.clone(),
            "/api/v1/projects",
            serde_json::json!({"repo_owner": "muri", "repo_name": "axofin"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["supported"], true);
        assert_eq!(json["template"], "rails");
        assert_eq!(json["branch"], "main");
        assert_eq!(json["has_dockerfile"], false);
        assert_eq!(json["files"].as_array().unwrap().len(), 4);
        let p = api.store.project_by_slug("axofin").unwrap().unwrap();
        assert_eq!(p.source, "registered");
        assert_eq!(p.status.as_deref(), Some("provisioning"));

        // unsupported: python repo comes back supported=false and is NOT registered
        let (status, json) = post_json(
            api.clone(),
            "/api/v1/projects",
            serde_json::json!({"repo_owner": "muri", "repo_name": "notas"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["supported"], false);
        assert_eq!(json["language"], "Python");
        assert!(api.store.project_by_slug("notas").unwrap().is_none());

        // provision: one commit through the git data chain + secrets
        let (status, json) = post_json(api.clone(), "/api/v1/projects/axofin/provision", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["commit_sha"], "a3f9e21fff");
        assert_eq!(json["files"].as_array().unwrap().len(), 4);
        let secrets = json["secrets"].as_array().unwrap();
        assert_eq!(secrets.iter().find(|s| s["name"] == "WEBO_DEPLOY_TOKEN").unwrap()["status"], "created");
        assert_eq!(secrets.iter().find(|s| s["name"] == "TS_OAUTH_CLIENT_ID").unwrap()["status"], "skipped");
        let p = api.store.project_by_slug("axofin").unwrap().unwrap();
        assert_eq!(p.status.as_deref(), Some("deploying"));

        // bad input
        let (status, _) = post_json(
            api.clone(),
            "/api/v1/projects",
            serde_json::json!({"repo_owner": "../evil", "repo_name": "x"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        std::env::remove_var("WEBO_GITHUB_API_BASE");
        std::env::remove_var("WEBO_GITHUB_TOKEN");
        std::env::remove_var("WEBO_DEPLOY_TOKEN");
    }
}
