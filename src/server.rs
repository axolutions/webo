use crate::metrics::State;
use crate::store::Store;
use crate::{cloudflare, db, github, projects, scaffold};
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
        .route("/api/v1/projects/{slug}", get(project_detail).delete(project_delete))
        .route("/api/v1/projects/{slug}/provision", axum::routing::post(project_provision))
        .route("/api/v1/projects/{slug}/domain", axum::routing::put(domain_connect).delete(domain_disconnect))
        .route("/api/v1/projects/{slug}/database", get(database_get).post(database_create).delete(database_drop))
        .route("/api/v1/projects/{slug}/database/tables", get(database_tables))
        .route("/api/v1/projects/{slug}/database/query", axum::routing::post(database_query))
        .route("/api/v1/projects/{slug}/env", get(env_list).put(env_set).delete(env_delete))
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
                "url": p.custom_domain.clone().or_else(|| p.auto_domain.clone()).map(|h| format!("https://{h}")),
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
        "auto_domain": p.auto_domain,
        "custom_domain": p.custom_domain,
        "port": p.port,
        "tunnel_target": cloudflare::Cloudflare::from_env().map(|cf| cf.tunnel_target()),
        "domains_available": cloudflare::Cloudflare::from_env().is_some(),
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
            // Secrets FIRST: the commit triggers the workflow immediately, and a
            // job reads its secrets at start — writing them after would race.
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
            let sha = github::commit_files(
                &token, &owner, &name, &scan.branch, &files,
                &format!("chore: webo scaffold ({label})"),
            )?;
            Ok::<_, String>((sha, files, secrets))
        }
    })
    .await;

    match result {
        Ok(Ok((sha, files, secrets))) => {
            let _ = api.store.set_status(&slug, Some("deploying"));
            let domain = reserve_domain(&api, &slug).await;
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
                "auto_domain": domain,
            }))
            .into_response()
        }
        Ok(Err(msg)) => err(StatusCode::BAD_GATEWAY, &msg),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "provision task failed"),
    }
}

async fn project_delete(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
    Json(opts): Json<projects::TeardownOpts>,
) -> impl IntoResponse {
    if slug == "webo" {
        return err(StatusCode::FORBIDDEN, "webo cannot delete itself");
    }
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let compose = p.compose_project.clone().unwrap_or_else(|| p.slug.clone());
    let report = projects::teardown(&compose, opts).await;
    // release the hostnames before forgetting the project
    for host in [p.auto_domain.clone(), p.custom_domain.clone()].into_iter().flatten() {
        if let Some(cf) = cloudflare::Cloudflare::from_env() {
            if cloudflare::split_host(&host, &cf.apps_zone).is_some() {
                let _ = tokio::task::spawn_blocking(move || {
                    cloudflare::Cloudflare::from_env().map(|cf| cf.delete_dns(&host))
                })
                .await;
            }
        }
    }
    let _ = api.store.delete_project(&slug);
    api.state.write().await.projects_live.remove(&slug);
    sync_routes(&api).await;
    Json(serde_json::json!({
        "deleted": true,
        "containers_removed": report.containers_removed,
        "volumes_removed": report.volumes_removed,
        "images_removed": report.images_removed,
    }))
    .into_response()
}

/// Reserves the project's auto domain (once) and publishes its tunnel route.
/// Everything here is best-effort: without Cloudflare configured the project
/// is created all the same, just without a URL.
async fn reserve_domain(api: &Api, slug: &str) -> Option<String> {
    let cf = cloudflare::Cloudflare::from_env()?;
    let existing = api.store.project_by_slug(slug).ok().flatten()?.auto_domain;
    let host = match existing {
        Some(h) => h,
        None => {
            let taken: Vec<String> = api
                .store
                .projects()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| p.auto_domain)
                .collect();
            let mut label = cloudflare::random_label_os();
            for _ in 0..5 {
                let candidate = format!("{label}.{}", cf.apps_zone);
                if !taken.contains(&candidate) {
                    break;
                }
                label = cloudflare::random_label_os();
            }
            let host = format!("{label}.{}", cf.apps_zone);
            let slug_owned = slug.to_string();
            let label_owned = label.clone();
            let created = tokio::task::spawn_blocking(move || {
                cloudflare::Cloudflare::from_env()
                    .map(|cf| cf.create_dns(&label_owned, &format!("webo: {slug_owned}")))
            })
            .await
            .ok()
            .flatten();
            if !matches!(created, Some(Ok(()))) {
                return None;
            }
            let _ = api.store.set_auto_domain_if_empty(slug, &host, 3000);
            host
        }
    };
    sync_routes(api).await;
    Some(host)
}

/// Pushes every hostname webo manages into the tunnel configuration,
/// preserving rules it does not own.
async fn sync_routes(api: &Api) {
    let Ok(routes) = api.store.routes() else { return };
    let _ = tokio::task::spawn_blocking(move || {
        let Some(cf) = cloudflare::Cloudflare::from_env() else { return };
        if let Ok(current) = cf.ingress() {
            let merged = cloudflare::merge_ingress(&current, &routes);
            let _ = cf.put_ingress(merged);
        }
    })
    .await;
}

#[derive(Deserialize)]
struct DomainReq {
    domain: String,
}

async fn domain_connect(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
    Json(req): Json<DomainReq>,
) -> impl IntoResponse {
    let Some(cf) = cloudflare::Cloudflare::from_env() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "cloudflare not configured");
    };
    let host = req.domain.trim().trim_start_matches("https://").trim_end_matches('/').to_string();
    if !cloudflare::valid_hostname(&host) {
        return err(StatusCode::BAD_REQUEST, "invalid domain");
    }
    if api.store.project_by_slug(&slug).ok().flatten().is_none() {
        return err(StatusCode::NOT_FOUND, "project not found");
    }
    // In our own zone webo creates the CNAME; elsewhere the user points it.
    let in_our_zone = cloudflare::split_host(&host, &cf.apps_zone).is_some();
    let dns = if in_our_zone {
        let label = cloudflare::split_host(&host, &cf.apps_zone).map(|(l, _)| l.to_string()).unwrap_or_default();
        let slug_owned = slug.clone();
        tokio::task::spawn_blocking(move || {
            cloudflare::Cloudflare::from_env().map(|cf| cf.create_dns(&label, &format!("webo: {slug_owned}")))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(Ok(()))
    } else {
        Ok(())
    };
    if let Err(e) = dns {
        return err(StatusCode::BAD_GATEWAY, &e);
    }
    let _ = api.store.set_custom_domain(&slug, Some(&host));
    sync_routes(&api).await;
    Json(serde_json::json!({
        "domain": host,
        "dns_managed": in_our_zone,
        "cname_target": cf.tunnel_target(),
    }))
    .into_response()
}

async fn domain_disconnect(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    if let (Some(host), Some(cf)) = (p.custom_domain.clone(), cloudflare::Cloudflare::from_env()) {
        if cloudflare::split_host(&host, &cf.apps_zone).is_some() {
            let _ = tokio::task::spawn_blocking(move || {
                cloudflare::Cloudflare::from_env().map(|cf| cf.delete_dns(&host))
            })
            .await;
        }
    }
    let _ = api.store.set_custom_domain(&slug, None);
    sync_routes(&api).await;
    Json(serde_json::json!({ "disconnected": true })).into_response()
}

// ---------- databases and environment variables ----------

fn app_network() -> String {
    std::env::var("WEBO_APP_NETWORK").unwrap_or_else(|_| "homelab".into())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Writes the project's variables into ~/apps/<slug>/.env on the server, which
/// the compose files already read through `env_file`.
async fn materialize_env(api: &Api, slug: &str) -> Result<(), String> {
    let Ok(Some(p)) = api.store.project_by_slug(slug) else { return Err("project not found".into()) };
    let vars = api.store.env_vars(p.id).map_err(|e| e.to_string())?;
    let body: String = vars.iter().map(|v| format!("{}={}\n", v.key, v.value)).collect();
    let app = p.compose_project.clone().unwrap_or_else(|| p.slug.clone());
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(format!("mkdir -p ~/apps/{app} && cat > ~/apps/{app}/.env"))
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child.stdin.as_mut().ok_or("no stdin")?.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        child.wait().map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn database_get(AxumState(api): AxumState<Api>, AxumPath(slug): AxumPath<String>) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let stored = api.store.database(p.id).ok().flatten();
    // no managed database? a SQLite file may still be sitting in a volume
    let db = match stored {
        Some(d) => Some(d),
        None => {
            let found = db::detect_sqlite(&p.compose_project.clone().unwrap_or_else(|| p.slug.clone())).await;
            if let Some(d) = &found {
                let _ = api.store.set_database(p.id, d);
            }
            found
        }
    };
    Json(serde_json::json!({ "database": db })).into_response()
}

async fn database_create(AxumState(api): AxumState<Api>, AxumPath(slug): AxumPath<String>) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    if api.store.database(p.id).ok().flatten().is_some() {
        return err(StatusCode::CONFLICT, "this project already has a database");
    }
    let compose = p.compose_project.clone().unwrap_or_else(|| p.slug.clone());
    match db::create_postgres(&compose, &app_network()).await {
        Ok(mut database) => {
            database.created_at = now_secs();
            let url = db::database_url(
                database.username.as_deref().unwrap_or_default(),
                database.password.as_deref().unwrap_or_default(),
                database.container.as_deref().unwrap_or_default(),
                database.db_name.as_deref().unwrap_or_default(),
            );
            let _ = api.store.set_database(p.id, &database);
            let _ = api.store.set_env(p.id, "DATABASE_URL", &url, true);
            let materialized = materialize_env(&api, &slug).await.is_ok();
            Json(serde_json::json!({
                "database": database,
                "env_written": materialized,
                "restart_needed": true,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &e),
    }
}

async fn database_drop(AxumState(api): AxumState<Api>, AxumPath(slug): AxumPath<String>) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let Some(database) = api.store.database(p.id).ok().flatten() else {
        return err(StatusCode::NOT_FOUND, "project has no database");
    };
    if database.kind == "postgres" {
        if let Some(c) = database.container.as_deref() {
            let _ = db::drop_postgres(c, database.volume.as_deref()).await;
        }
    }
    let _ = api.store.delete_database(p.id);
    let _ = api.store.delete_env(p.id, "DATABASE_URL");
    Json(serde_json::json!({ "dropped": true })).into_response()
}

async fn run_sql(api: &Api, slug: &str, sql: &str, write: bool) -> Result<String, (StatusCode, String)> {
    let Ok(Some(p)) = api.store.project_by_slug(slug) else {
        return Err((StatusCode::NOT_FOUND, "project not found".into()));
    };
    let Some(database) = api.store.database(p.id).ok().flatten() else {
        return Err((StatusCode::NOT_FOUND, "project has no database".into()));
    };
    let out = if database.kind == "postgres" {
        db::pg_query(&database, &app_network(), sql, write).await
    } else {
        db::sqlite_query(&database, sql, write).await
    };
    out.map_err(|e| (StatusCode::BAD_GATEWAY, e))
}

async fn database_tables(AxumState(api): AxumState<Api>, AxumPath(slug): AxumPath<String>) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let Some(database) = api.store.database(p.id).ok().flatten() else {
        return err(StatusCode::NOT_FOUND, "project has no database");
    };
    let sql = if database.kind == "postgres" {
        "SELECT table_name FROM information_schema.tables WHERE table_schema='public' ORDER BY table_name"
    } else {
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    };
    match run_sql(&api, &slug, sql, false).await {
        Ok(out) => {
            let parsed = db::parse_table_output(&out, 500);
            let tables: Vec<String> = parsed.rows.iter().filter_map(|r| r.first().cloned()).collect();
            Json(serde_json::json!({ "kind": database.kind, "tables": tables })).into_response()
        }
        Err((code, msg)) => err(code, &msg),
    }
}

#[derive(Deserialize)]
struct QueryReq {
    sql: String,
    #[serde(default)]
    write: bool,
}

async fn database_query(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
    Json(req): Json<QueryReq>,
) -> impl IntoResponse {
    let sql = req.sql.trim().to_string();
    if sql.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty statement");
    }
    if db::is_write_statement(&sql) && !req.write {
        return err(StatusCode::FORBIDDEN, "this statement changes data — turn write mode on");
    }
    match run_sql(&api, &slug, &sql, req.write).await {
        Ok(out) => {
            let parsed = db::parse_table_output(&out, 200);
            Json(serde_json::json!({
                "columns": parsed.columns,
                "rows": parsed.rows,
                "row_count": parsed.row_count,
                "truncated": parsed.truncated,
                "raw": out.chars().take(4000).collect::<String>(),
            }))
            .into_response()
        }
        Err((code, msg)) => err(code, &msg),
    }
}

async fn env_list(AxumState(api): AxumState<Api>, AxumPath(slug): AxumPath<String>) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let defined = api.store.env_vars(p.id).unwrap_or_default();
    // what the repo says it needs, so a missing one shows before the deploy breaks
    let expected = match (p.repo_owner.clone(), p.repo_name.clone(), github_token()) {
        (Some(owner), Some(name), Some(token)) => {
            tokio::task::spawn_blocking(move || github::expected_env_vars(&token, &owner, &name))
                .await
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let defined_keys: Vec<&str> = defined.iter().map(|v| v.key.as_str()).collect();
    let missing: Vec<&String> = expected.iter().filter(|k| !defined_keys.contains(&k.as_str())).collect();
    Json(serde_json::json!({
        "vars": defined.iter().map(|v| serde_json::json!({
            "key": v.key,
            "managed": v.managed,
            "preview": mask(&v.value),
        })).collect::<Vec<_>>(),
        "expected": expected,
        "missing": missing,
    }))
    .into_response()
}

fn mask(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        "•".repeat(n.max(4))
    } else {
        format!("{}…{}", value.chars().take(4).collect::<String>(), "•".repeat(6))
    }
}

#[derive(Deserialize)]
struct EnvReq {
    key: String,
    value: String,
}

async fn env_set(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
    Json(req): Json<EnvReq>,
) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let key = req.key.trim().to_string();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return err(StatusCode::BAD_REQUEST, "invalid variable name");
    }
    if api.store.env_vars(p.id).unwrap_or_default().iter().any(|v| v.key == key && v.managed) {
        return err(StatusCode::FORBIDDEN, "this variable is managed by webo");
    }
    let _ = api.store.set_env(p.id, &key, &req.value, false);
    let written = materialize_env(&api, &slug).await.is_ok();
    Json(serde_json::json!({ "key": key, "env_written": written, "restart_needed": true })).into_response()
}

#[derive(Deserialize)]
struct EnvKey {
    key: String,
}

async fn env_delete(
    AxumState(api): AxumState<Api>,
    AxumPath(slug): AxumPath<String>,
    Query(q): Query<EnvKey>,
) -> impl IntoResponse {
    let Ok(Some(p)) = api.store.project_by_slug(&slug) else {
        return err(StatusCode::NOT_FOUND, "project not found");
    };
    let removed = api.store.delete_env(p.id, &q.key).unwrap_or(false);
    if !removed {
        return err(StatusCode::FORBIDDEN, "managed variables go with their database");
    }
    let _ = materialize_env(&api, &slug).await;
    Json(serde_json::json!({ "deleted": true })).into_response()
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

    async fn delete_json(api: Api, path: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let res = app(api)
            .oneshot(
                Request::builder()
                    .method("DELETE")
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

    #[tokio::test]
    async fn delete_refuses_webo_and_unknown() {
        let api = api_with_data();
        api.store.upsert_discovered("webo", "webo", None, None, 1).unwrap();
        let (status, json) = delete_json(api.clone(), "/api/v1/projects/webo", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json["error"], "webo cannot delete itself");
        let (status, _) = delete_json(api.clone(), "/api/v1/projects/nope", serde_json::json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_removes_registration_and_live_state() {
        let api = api_with_data();
        let (status, json) = delete_json(
            api.clone(),
            "/api/v1/projects/codo",
            serde_json::json!({"containers": false, "volumes": false, "images": false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"], true);
        assert!(api.store.project_by_slug("codo").unwrap().is_none());
        assert!(!api.state.read().await.projects_live.contains_key("codo"));
    }

    #[tokio::test]
    async fn env_endpoints_guard_managed_vars_and_names() {
        let api = api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        api.store.set_env(id, "DATABASE_URL", "postgres://u:p@h:5432/d", true).unwrap();

        // listing masks values and never leaks the secret
        let (status, json) = get_json("/api/v1/projects/codo/env").await;
        assert_eq!(status, StatusCode::OK);
        let body = json.to_string();
        assert!(!body.contains("postgres://u:p"), "value never leaves the server");

        let put = |api: Api, body: &str| {
            let body = body.to_string();
            async move {
                app(api)
                    .oneshot(
                        Request::builder()
                            .method("PUT")
                            .uri("/api/v1/projects/codo/env")
                            .header("content-type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            }
        };
        // a managed variable cannot be overwritten by hand
        let res = put(api.clone(), r#"{"key":"DATABASE_URL","value":"x"}"#).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // nor deleted
        let res = app(api.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/projects/codo/env?key=DATABASE_URL")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        // invalid names are refused
        let res = put(api.clone(), r#"{"key":"minha chave","value":"x"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn query_refuses_writes_unless_write_mode_is_on() {
        let api = api_with_data();
        let post = |api: Api, body: &str| {
            let body = body.to_string();
            async move {
                app(api)
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/v1/projects/codo/database/query")
                            .header("content-type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            }
        };
        let res = post(api.clone(), r#"{"sql":"DELETE FROM users","write":false}"#).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["error"].as_str().unwrap().contains("write mode"));

        // empty input is refused before touching anything
        let res = post(api.clone(), r#"{"sql":"   "}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // a read on a project without a database says so plainly
        let res = post(api.clone(), r#"{"sql":"SELECT 1"}"#).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn database_endpoints_404_for_unknown_project() {
        let (status, _) = get_json("/api/v1/projects/nope/database/tables").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let res = app(api_with_data())
            .oneshot(Request::builder().method("DELETE").uri("/api/v1/projects/nope/database").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn domain_endpoints_need_cloudflare() {
        let _env = crate::testutil::env_lock();
        for k in ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_ZONE_ID", "WEBO_TUNNEL_ID", "WEBO_APPS_ZONE"] {
            std::env::remove_var(k);
        }
        let res = app(api_with_data())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/projects/codo/domain")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"domain":"loja.example.com"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        // and the detail says domains are unavailable instead of breaking
        let (status, json) = get_json("/api/v1/projects/codo").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["domains_available"], false);
        assert_eq!(json["auto_domain"], serde_json::Value::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn domain_connect_and_disconnect_against_a_mock_cloudflare() {
        let _env = crate::testutil::env_lock();
        use axum::routing::{delete as axdelete, get as axget, post as axpost, put as axput};
        use serde_json::json;
        let router = axum::Router::new()
            .route("/zones/{z}/dns_records", axpost(|| async { axum::Json(json!({"success": true, "result": {"id": "rec1"}})) })
                .get(|| async { axum::Json(json!({"success": true, "result": [{"id": "rec1"}]})) }))
            .route("/zones/{z}/dns_records/{id}", axdelete(|| async { axum::Json(json!({"success": true, "result": {}})) }))
            .route("/accounts/{a}/cfd_tunnel/{t}/configurations",
                axget(|| async { axum::Json(json!({"success": true, "result": {"config": {"ingress": [
                    {"hostname": "keep.example.com", "service": "http://keep:1"},
                    {"service": "http_status:404"}]}}})) })
                .put(|body: String| async move {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let rules = v["config"]["ingress"].as_array().unwrap();
                    // the untouched rule survives and the catch-all stays last
                    assert_eq!(rules[0]["hostname"], "keep.example.com");
                    assert_eq!(rules.last().unwrap()["service"], "http_status:404");
                    axum::Json(json!({"success": true, "result": {}}))
                }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        std::env::set_var("WEBO_CF_API_BASE", format!("http://{addr}"));
        std::env::set_var("CLOUDFLARE_API_TOKEN", "t");
        std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acc");
        std::env::set_var("CLOUDFLARE_ZONE_ID", "zone");
        std::env::set_var("WEBO_TUNNEL_ID", "tun");
        std::env::set_var("WEBO_APPS_ZONE", "example.com");

        let api = api_with_data();
        // a domain in our own zone: webo creates the DNS itself
        let res = app(api.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/projects/codo/domain")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"domain":"https://loja.example.com/"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["domain"], "loja.example.com", "scheme and slash trimmed");
        assert_eq!(json["dns_managed"], true);
        assert_eq!(json["cname_target"], "tun.cfargotunnel.com");
        assert_eq!(api.store.project_by_slug("codo").unwrap().unwrap().custom_domain.as_deref(), Some("loja.example.com"));

        // a third-party zone: webo only routes and hands over the CNAME target
        let res = app(api.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/projects/codo/domain")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"domain":"app.cliente.com.br"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["dns_managed"], false);

        // invalid input is refused
        let res = app(api.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/projects/codo/domain")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"domain":"nao e dominio"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // disconnect clears it
        let res = app(api.clone())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/v1/projects/codo/domain")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(api.store.project_by_slug("codo").unwrap().unwrap().custom_domain, None);

        for k in ["WEBO_CF_API_BASE", "CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_ZONE_ID", "WEBO_TUNNEL_ID", "WEBO_APPS_ZONE"] {
            std::env::remove_var(k);
        }
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
