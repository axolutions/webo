//! MCP server — phase 1: read-only tools, resources and one prompt.
//!
//! Two deliberate decisions:
//!
//! 1. **No SDK.** MCP over HTTP is JSON-RPC 2.0 with seven methods. Hand-rolling
//!    it costs ~200 lines, keeps the binary dependency-free (a stated value of
//!    this project) and leaves full control of the response text — which is
//!    where the value of a server like this actually lives.
//! 2. **No HTTP loopback.** The tools call the same store and state the panel's
//!    handlers call. One code path, so the panel and the agent can never
//!    disagree about what is true.
//!
//! Every tool here is read-only. Writing arrives in phase 3, behind explicit
//! parameters — the plan is in the artifact, not in this file.

use crate::fmt;
use crate::server::Api;
use axum::extract::State as AxumState;
use axum::response::{IntoResponse, Json};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn app(api: Api) -> Router {
    Router::new()
        .route("/mcp", post(rpc))
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .with_state(api)
}

// ---------------------------------------------------------------- JSON-RPC

async fn rpc(AxumState(api): AxumState<Api>, Json(req): Json<Value>) -> impl IntoResponse {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    // notifications carry no id and expect no answer
    if id.is_none() {
        return Json(json!({})).into_response();
    }

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
            "serverInfo": { "name": "webo", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_catalog() })),
        "tools/call" => call_tool(&api, &params).await,
        "resources/list" => Ok(json!({ "resources": resource_catalog() })),
        "resources/read" => read_resource(&api, &params).await,
        "prompts/list" => Ok(json!({ "prompts": prompt_catalog() })),
        "prompts/get" => get_prompt(&params),
        other => Err(format!("unknown method: {other}")),
    };

    match result {
        Ok(value) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": value })).into_response(),
        Err(message) => Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32603, "message": message }
        }))
        .into_response(),
    }
}

const INSTRUCTIONS: &str = "\
webo watches one self-hosted server and the projects running on it. Every tool \
here is read-only. Start with server_health for the machine, or list_projects \
to see what is deployed; then project_status for one project. When something \
is wrong, list_errors gives grouped issues and error_detail gives the stack \
trace; search_logs finds the lines around it. Read webo://runbook before \
suggesting any change to the server.";

/// A tool's answer: text the model reads.
fn text(body: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": body.into() }] })
}

fn arg_str(params: &Value, key: &str) -> Option<String> {
    params
        .get("arguments")
        .and_then(|a| a.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn arg_usize(params: &Value, key: &str, default: usize, max: usize) -> usize {
    params
        .get("arguments")
        .and_then(|a| a.get(key))
        .and_then(|v| v.as_u64())
        .map(|v| (v as usize).clamp(1, max))
        .unwrap_or(default)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Minutes covered by a window name. `None` means the whole persisted history.
fn window_minutes(window: Option<&str>) -> i64 {
    match window {
        Some("now") | Some("live") => 15,
        Some("7d") => 7 * 24 * 60,
        _ => 24 * 60,
    }
}

// ---------------------------------------------------------------- catalog

fn schema(props: Value, required: Vec<&str>) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn tool(name: &str, description: &str, input: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input,
        // every phase-1 tool is a pure read: the client can call it without asking
        "annotations": { "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false },
    })
}

const SLUG_DESC: &str = "The project slug, as listed by list_projects.";

fn tool_catalog() -> Vec<Value> {
    let window = json!({
        "type": "string", "enum": ["now", "24h", "7d"],
        "description": "Time window. 'now' is the last 15 minutes, '7d' reads the persisted samples. Defaults to 24h."
    });
    vec![
        tool(
            "server_health",
            "The whole machine in one answer: CPU, memory, disk, temperature, battery, network, \
             uptime, how many projects are running, and what Docker is taking on disk. Call this \
             first when asked how the server is.",
            schema(json!({}), vec![]),
        ),
        tool(
            "server_processes",
            "The most active process groups on the host, with CPU, memory, disk i/o and thread \
             count. Answers 'what is eating the machine'.",
            schema(
                json!({
                    "filter": { "type": "string", "description": "Only groups whose name or command contains this." },
                    "sort_by": { "type": "string", "enum": ["cpu", "memory", "disk"], "description": "Defaults to cpu." },
                    "limit": { "type": "integer", "description": "How many groups to return (default 12, max 40)." }
                }),
                vec![],
            ),
        ),
        tool(
            "list_projects",
            "Every project on the server: technology, whether it is up, domain, CPU and memory, \
             open errors and the last deploy. One line each.",
            schema(json!({}), vec![]),
        ),
        tool(
            "project_status",
            "The full picture of one project: each container with uptime and restarts, disk \
             footprint, domains, latest builds, the version running, open errors and whether it \
             has a database. Answers 'how is X' without a follow-up call.",
            schema(json!({ "slug": { "type": "string", "description": SLUG_DESC } }), vec!["slug"]),
        ),
        tool(
            "project_metrics",
            "CPU, memory and disk for a project over a window, already summarised: average, peak \
             with the time it happened, and the trend. Returns a decimated sample, never the raw \
             series.",
            schema(
                json!({ "slug": { "type": "string", "description": SLUG_DESC }, "window": window.clone() }),
                vec!["slug"],
            ),
        ),
        tool(
            "search_logs",
            "Full-text search over a project's indexed logs, filterable by level, resource and \
             window. Returns the matching lines plus counts per level and the busiest hour, so a \
             single call shows both what happened and when it started.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "query": { "type": "string", "description": "Full-text query. Omit to see everything in the window." },
                    "level": { "type": "string", "enum": ["info", "warn", "error"], "description": "Only lines at this level." },
                    "resource": { "type": "string", "description": "Only this container, by name." },
                    "window": window.clone(),
                    "limit": { "type": "integer", "description": "How many lines (default 40, max 200)." }
                }),
                vec!["slug"],
            ),
        ),
        tool(
            "tail_logs",
            "The last lines straight from a container, bypassing the index — for seeing what is \
             happening right now, including lines the collector has not picked up yet.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "resource": { "type": "string", "description": "Container name. Defaults to the project's app container." },
                    "lines": { "type": "integer", "description": "How many lines (default 40, max 200)." }
                }),
                vec!["slug"],
            ),
        ),
        tool(
            "list_errors",
            "Issues grouped by cause, with occurrence count, the file to blame, first and last \
             seen, and whether they came from the server or a visitor's browser.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "state": { "type": "string", "enum": ["open", "resolved", "ignored", "all"], "description": "Defaults to open." }
                }),
                vec!["slug"],
            ),
        ),
        tool(
            "error_detail",
            "The occurrences of one issue with the full stack trace and where it came from. This \
             is what makes a fix suggestable.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "issue_id": { "type": "integer", "description": "The issue id from list_errors." }
                }),
                vec!["slug", "issue_id"],
            ),
        ),
    ]
}

fn resource_catalog() -> Vec<Value> {
    vec![
        json!({
            "uri": "webo://runbook",
            "name": "Server runbook",
            "description": "How this server is put together and the lessons learned the hard way. \
                            Read before suggesting any change.",
            "mimeType": "text/markdown",
        }),
        json!({
            "uri": "webo://projects",
            "name": "Project inventory",
            "description": "Compact list of every project, so a slug never has to be guessed.",
            "mimeType": "text/plain",
        }),
    ]
}

fn prompt_catalog() -> Vec<Value> {
    vec![json!({
        "name": "diagnose_project",
        "description": "Work out why a project is failing or slow, in the order that avoids dead ends.",
        "arguments": [{ "name": "slug", "description": SLUG_DESC, "required": true }],
    })]
}

// ---------------------------------------------------------------- tools

async fn call_tool(api: &Api, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    match name {
        "server_health" => Ok(text(server_health(api).await)),
        "server_processes" => Ok(text(server_processes(api, params).await)),
        "list_projects" => Ok(text(list_projects(api).await)),
        "project_status" => run(api, params, project_status).await,
        "project_metrics" => run(api, params, project_metrics).await,
        "search_logs" => run(api, params, search_logs).await,
        "tail_logs" => run(api, params, tail_logs).await,
        "list_errors" => run(api, params, list_errors).await,
        "error_detail" => run(api, params, error_detail).await,
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Shared shape for the project tools: resolve the slug once, and answer with
/// a usable error (naming the projects that do exist) when it is wrong.
async fn run<F, Fut>(api: &Api, params: &Value, f: F) -> Result<Value, String>
where
    F: FnOnce(Api, crate::store::Project, Value) -> Fut,
    Fut: std::future::Future<Output = String>,
{
    let Some(slug) = arg_str(params, "slug") else {
        return Err("this tool needs a slug — call list_projects to see them".into());
    };
    match api.store.project_by_slug(&slug) {
        Ok(Some(p)) => Ok(text(f(api.clone(), p, params.clone()).await)),
        _ => {
            let known: Vec<String> = api
                .store
                .projects()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.slug)
                .collect();
            Err(format!("no project named '{slug}'. Existing: {}", known.join(", ")))
        }
    }
}

async fn server_health(api: &Api) -> String {
    let (s, sys, live, containers, d) = {
        let st = api.state.read().await;
        (
            st.snapshot.clone(),
            st.system.clone(),
            st.projects_live.len(),
            st.projects_live.values().map(|l| l.containers.len()).sum::<usize>(),
            st.docker,
        )
    };
    if s.ts == 0 {
        return "The collector has not produced a sample yet — try again in a few seconds.".into();
    }
    let mem_pct = if s.mem_total > 0 { s.mem_used as f32 / s.mem_total as f32 * 100.0 } else { 0.0 };
    let disk_pct = if s.disk_total > 0 { s.disk_used as f32 / s.disk_total as f32 * 100.0 } else { 0.0 };

    let temp = match s.temp_c {
        Some(t) => {
            let state = if t >= 85.0 { "CRITICAL" } else if t >= 78.0 { "warm" } else { "normal" };
            let fan = s.fan_rpm.map(|r| format!(", fan {r} rpm")).unwrap_or_default();
            format!("{t:.0}°C ({state}{fan})")
        }
        None => "not exposed by this machine".into(),
    };
    let battery = match s.battery_pct {
        Some(p) => {
            let status = s.battery_status.as_deref().unwrap_or("unknown");
            let limit = s.battery_limit_pct.map(|l| format!(", charge capped at {l}%")).unwrap_or_default();
            format!("{p}% ({status}{limit})")
        }
        None => "none".into(),
    };

    let projects = api.store.projects().unwrap_or_default();
    let open_errors: i64 = projects
        .iter()
        .map(|p| api.store.open_issue_count(p.id).unwrap_or(0))
        .sum();

    format!(
        "{host} · {os} · kernel {kernel} · up {uptime}\n\
         \n\
         CPU      {cpu} of {threads} threads ({brand}), load {load:.2}\n\
         Memory   {mem_used} of {mem_total} used ({mem_pct})\n\
         Disk     {disk_used} of {disk_total} used ({disk_pct}), {disk_free} free\n\
         Temp     {temp}\n\
         Battery  {battery}\n\
         Network  down {rx}, up {tx}\n\
         \n\
         Projects {total} registered, {live} running, {containers} containers, {errors} open errors\n\
         Docker   {imgs} images ({imgs_b}), {vols} volumes ({vols_b}), {reclaim} reclaimable\n\
         \n\
         Sampled every {sample}s; webo v{ver}. Metrics as of {stamp}.",
        host = sys.hostname,
        os = sys.os,
        kernel = sys.kernel,
        uptime = fmt::duration(s.uptime_secs),
        cpu = fmt::pct(s.cpu_pct),
        threads = s.cpu_threads,
        brand = sys.cpu_brand,
        load = s.load_1m,
        mem_used = fmt::bytes(s.mem_used),
        mem_total = fmt::bytes(s.mem_total),
        mem_pct = fmt::pct(mem_pct),
        disk_used = fmt::bytes(s.disk_used),
        disk_total = fmt::bytes(s.disk_total),
        disk_pct = fmt::pct(disk_pct),
        disk_free = fmt::bytes(s.disk_total.saturating_sub(s.disk_used)),
        rx = fmt::bytes_per_sec(s.net_rx_bps),
        tx = fmt::bytes_per_sec(s.net_tx_bps),
        total = projects.len(),
        live = live,
        containers = containers,
        errors = open_errors,
        imgs = d.images,
        imgs_b = fmt::bytes(d.images_bytes),
        vols = d.volumes,
        vols_b = fmt::bytes(d.volumes_bytes),
        reclaim = fmt::bytes(d.reclaimable_bytes),
        sample = sys.sample_secs,
        ver = sys.webo_version,
        stamp = fmt::clock(s.ts as i64),
    )
}

async fn server_processes(api: &Api, params: &Value) -> String {
    let filter = arg_str(params, "filter").map(|f| f.to_lowercase());
    let sort_by = arg_str(params, "sort_by").unwrap_or_else(|| "cpu".into());
    let limit = arg_usize(params, "limit", 12, 40);
    let st = api.state.read().await;
    let mut list: Vec<_> = st
        .processes
        .iter()
        .filter(|p| {
            filter.as_ref().is_none_or(|f| {
                p.name.to_lowercase().contains(f) || p.cmd.to_lowercase().contains(f)
            })
        })
        .collect();
    match sort_by.as_str() {
        "memory" => list.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes)),
        "disk" => list.sort_by(|a, b| b.disk_bps.cmp(&a.disk_bps)),
        _ => list.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct)),
    }
    let total_shown = list.len();
    if total_shown == 0 {
        return match filter {
            Some(f) => format!("No process group matches '{f}'."),
            None => "The collector has not scanned processes yet.".into(),
        };
    }
    let cpu_sum: f32 = st.processes.iter().map(|p| p.cpu_pct).sum();
    let mem_sum: u64 = st.processes.iter().map(|p| p.mem_bytes).sum();
    let rows: Vec<String> = list
        .iter()
        .take(limit)
        .map(|p| {
            let procs = if p.procs > 1 { format!(" ({} procs)", p.procs) } else { String::new() };
            format!(
                "{name:<22} cpu {cpu:>6}  ram {mem:>9}  disk {disk:>10}  {thr:>3} thr  up {up}{procs}\n  {cmd}",
                name = p.name.chars().take(22).collect::<String>(),
                cpu = fmt::pct(p.cpu_pct),
                mem = fmt::bytes(p.mem_bytes),
                disk = fmt::bytes_per_sec(p.disk_bps),
                thr = p.threads,
                up = fmt::duration(p.uptime_secs),
                procs = procs,
                cmd = p.cmd.chars().take(96).collect::<String>(),
            )
        })
        .collect();
    format!(
        "{shown} of {groups} process groups, sorted by {sort}. Host totals: cpu {cpu_sum}, ram {mem_sum}.\n\n{rows}",
        shown = rows.len(),
        groups = total_shown,
        sort = sort_by,
        cpu_sum = fmt::pct(cpu_sum),
        mem_sum = fmt::bytes(mem_sum),
        rows = rows.join("\n"),
    )
}

async fn list_projects(api: &Api) -> String {
    let projects = api.store.projects().unwrap_or_default();
    if projects.is_empty() {
        return "No projects yet — nothing is registered or running on this server.".into();
    }
    let st = api.state.read().await;
    let n = now();
    let mut rows: Vec<(i64, String)> = projects
        .iter()
        .map(|p| {
            let live = st.projects_live.get(&p.slug);
            let errors = api.store.open_issue_count(p.id).unwrap_or(0);
            let last_build = api.store.builds(p.id, 1).ok().and_then(|b| b.into_iter().next());
            let state = match (&p.status, live.is_some()) {
                (Some(s), _) => s.clone(),
                (None, true) => "up".into(),
                (None, false) => "stopped".into(),
            };
            let domain = p
                .custom_domain
                .clone()
                .or_else(|| p.auto_domain.clone())
                .or_else(|| p.domain.clone())
                .unwrap_or_else(|| "no domain".into());
            let deploy = match &last_build {
                Some(b) => format!("deploy {} ({})", fmt::ago(b.created_at, n), &b.commit_sha[..7.min(b.commit_sha.len())]),
                None => "never deployed".into(),
            };
            let line = format!(
                "{slug:<24} {state:<13} {tech:<9} cpu {cpu:>6} ram {mem:>9} {res} res · {errs} · {deploy}\n  {domain}",
                slug = p.slug,
                state = state,
                tech = p.tech.clone().unwrap_or_else(|| "-".into()),
                cpu = fmt::pct(live.map(|l| l.cpu_pct).unwrap_or(0.0)),
                mem = fmt::bytes(live.map(|l| l.mem_bytes).unwrap_or(0)),
                res = live.map(|l| l.containers.len()).unwrap_or(0),
                errs = if errors > 0 { format!("{errors} open errors") } else { "no errors".into() },
                deploy = deploy,
                domain = domain,
            );
            // projects needing attention first: errors, then stopped, then name
            let rank = if errors > 0 { 0 } else if live.is_none() { 1 } else { 2 };
            (rank, line)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    format!(
        "{} projects, needing attention first:\n\n{}",
        rows.len(),
        rows.into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n")
    )
}

async fn project_status(api: Api, p: crate::store::Project, _params: Value) -> String {
    let st = api.state.read().await;
    let live = st.projects_live.get(&p.slug).cloned().unwrap_or_default();
    drop(st);
    let n = now();
    let issues = api.store.issues(p.id, Some("open")).unwrap_or_default();
    let builds = api.store.builds(p.id, 5).unwrap_or_default();
    let versions = api.store.versions(p.id, 8).unwrap_or_default();
    let database = api.store.database(p.id).ok().flatten();

    let up = live.containers.iter().map(|c| c.uptime_secs).max().unwrap_or(0);
    let restarts: i64 = live.containers.iter().map(|c| c.restarts).sum();
    let header = if live.containers.is_empty() {
        format!("{} — NOT RUNNING", p.slug)
    } else {
        format!("{} — up {}, {} restarts", p.slug, fmt::duration(up), restarts)
    };

    let resources = if live.containers.is_empty() {
        "  none running".to_string()
    } else {
        live.containers
            .iter()
            .map(|c| {
                format!(
                    "  {name:<26} {role:<9} cpu {cpu:>6} ram {mem:>9} restarts {r}  up {up}\n    {img}",
                    name = c.name,
                    role = c.role,
                    cpu = fmt::pct(c.cpu_pct),
                    mem = fmt::bytes(c.mem_bytes),
                    r = c.restarts,
                    up = fmt::duration(c.uptime_secs),
                    img = c.image,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let domains = {
        let mut d = Vec::new();
        if let Some(a) = &p.auto_domain {
            d.push(format!("  {a} (automatic, never changes)"));
        }
        if let Some(c) = &p.custom_domain {
            d.push(format!("  {c} (custom)"));
        }
        if d.is_empty() {
            d.push("  none".into());
        }
        d.join("\n")
    };

    let db_line = match &database {
        Some(d) if d.kind == "postgres" => format!(
            "  postgres in its own container ({}), database {}",
            d.container.clone().unwrap_or_default(),
            d.db_name.clone().unwrap_or_default()
        ),
        Some(d) => format!(
            "  sqlite at {} in volume {}{}",
            d.file_path.clone().unwrap_or_default(),
            d.volume.clone().unwrap_or_default(),
            if d.persisted { "" } else { " — NOT PERSISTED, every deploy wipes it" }
        ),
        None => "  none".into(),
    };

    let build_lines = if builds.is_empty() {
        "  never deployed".to_string()
    } else {
        builds
            .iter()
            .map(|b| {
                let outcome = b.conclusion.clone().unwrap_or_else(|| b.status.clone());
                format!(
                    "  {mark} {sha}  {dur}s  {ago}  {msg}",
                    mark = if outcome == "success" { "ok  " } else { "FAIL" },
                    sha = &b.commit_sha[..7.min(b.commit_sha.len())],
                    dur = b.duration_secs,
                    ago = fmt::ago(b.created_at, n),
                    msg = b.commit_msg.lines().next().unwrap_or("").chars().take(60).collect::<String>(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let errors_line = if issues.is_empty() {
        "  none open".to_string()
    } else {
        let mut lines = vec![format!("  {} open, most recent first:", issues.len())];
        for i in issues.iter().take(5) {
            lines.push(format!(
                "  · {count}x [{src}] {title}{culprit}",
                count = i.count,
                src = i.source,
                title = i.title.chars().take(72).collect::<String>(),
                culprit = i.culprit.as_ref().map(|c| format!(" — {c}")).unwrap_or_default(),
            ));
        }
        lines.push("  use list_errors and error_detail for the stack traces".into());
        lines.join("\n")
    };

    format!(
        "{header}\n\
         repository  {repo}\n\
         technology  {tech}\n\
         footprint   {size} on disk (image {img} + volumes {vol})\n\
         version     {version}\n\
         \n\
         Resources\n{resources}\n\
         \n\
         Domains\n{domains}\n\
         \n\
         Database\n{db}\n\
         \n\
         Recent deploys\n{builds}\n\
         \n\
         Errors\n{errors}",
        header = header,
        repo = p
            .repo_owner
            .as_ref()
            .zip(p.repo_name.as_ref())
            .map(|(o, r)| format!("{o}/{r}"))
            .unwrap_or_else(|| "not connected".into()),
        tech = p.tech.clone().unwrap_or_else(|| "unknown".into()),
        size = fmt::bytes(live.image_bytes + live.volume_bytes),
        img = fmt::bytes(live.image_bytes),
        vol = fmt::bytes(live.volume_bytes),
        version = versions
            .iter()
            .find(|v| v.current)
            .map(|v| v.tag.clone())
            .unwrap_or_else(|| "unknown".into()),
        resources = resources,
        domains = domains,
        db = db_line,
        builds = build_lines,
        errors = errors_line,
    )
}

async fn project_metrics(api: Api, p: crate::store::Project, params: Value) -> String {
    let window = arg_str(&params, "window").unwrap_or_else(|| "24h".into());
    let minutes = window_minutes(Some(&window));
    let n = now();

    // 7d comes from the persisted aggregates; shorter windows from live history
    let series: Vec<(i64, f32, u64, u64)> = if window == "7d" {
        api.store
            .samples(&format!("project:{}", p.slug), n - minutes * 60)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.ts, s.cpu_pct as f32, s.mem_bytes as u64, s.disk_bps as u64))
            .collect()
    } else {
        let st = api.state.read().await;
        let cutoff = (n - minutes * 60) as u64;
        st.projects_live
            .get(&p.slug)
            .map(|l| {
                l.history
                    .iter()
                    .filter(|h| h.ts >= cutoff)
                    .map(|h| (h.ts as i64, h.cpu_pct, h.mem_bytes, h.disk_bps))
                    .collect()
            })
            .unwrap_or_default()
    };

    if series.is_empty() {
        return format!(
            "No samples for {} in the {window} window.{}",
            p.slug,
            if window == "7d" {
                " The 7-day series is built from 5-minute aggregates, so a project deployed \
                 in the last few minutes has none yet."
            } else {
                " The project may not be running."
            }
        );
    }

    let cpu: Vec<(i64, f64)> = series.iter().map(|(t, c, _, _)| (*t, *c as f64)).collect();
    let mem: Vec<(i64, f64)> = series.iter().map(|(t, _, m, _)| (*t, *m as f64)).collect();
    let disk: Vec<(i64, f64)> = series.iter().map(|(t, _, _, d)| (*t, *d as f64)).collect();

    let sample = fmt::decimate(&series, 12);
    let sample_lines: Vec<String> = sample
        .iter()
        .map(|(ts, c, m, d)| {
            format!(
                "  {}  cpu {:>6}  ram {:>9}  disk {}",
                fmt::clock(*ts),
                fmt::pct(*c),
                fmt::bytes(*m),
                fmt::bytes_per_sec(*d)
            )
        })
        .collect();

    format!(
        "{slug} · {window} window · {span}\n\n{cpu}\n{mem}\n{disk}\n\nSample ({n} of {total} points):\n{sample}",
        slug = p.slug,
        window = window,
        span = format!("{} → now", fmt::clock(series[0].0)),
        cpu = fmt::series_line("CPU   ", &cpu, n, |v| fmt::pct(v as f32)),
        mem = fmt::series_line("RAM   ", &mem, n, |v| fmt::bytes(v as u64)),
        disk = fmt::series_line("Disk  ", &disk, n, |v| fmt::bytes_per_sec(v as u64)),
        n = sample.len(),
        total = series.len(),
        sample = sample_lines.join("\n"),
    )
}

async fn search_logs(api: Api, p: crate::store::Project, params: Value) -> String {
    let query = arg_str(&params, "query");
    let level = arg_str(&params, "level");
    let resource = arg_str(&params, "resource");
    let window = arg_str(&params, "window");
    let limit = arg_usize(&params, "limit", 40, 200);
    let n = now();
    let since = n - window_minutes(window.as_deref()) * 60;

    let all = api
        .store
        .search_logs(p.id, query.as_deref(), resource.as_deref(), Some(since), 5000)
        .unwrap_or_default();
    let (mut info, mut warn, mut error) = (0u64, 0u64, 0u64);
    let mut per_hour: std::collections::BTreeMap<i64, u64> = std::collections::BTreeMap::new();
    for l in &all {
        match crate::errors::level_of(&l.line, &l.stream) {
            "error" => error += 1,
            "warn" => warn += 1,
            _ => info += 1,
        }
        *per_hour.entry(l.ts / 3600 * 3600).or_default() += 1;
    }
    let shown: Vec<&crate::store::LogLine> = all
        .iter()
        .filter(|l| {
            level
                .as_deref()
                .is_none_or(|lv| crate::errors::level_of(&l.line, &l.stream) == lv)
        })
        .take(limit)
        .collect();

    if all.is_empty() {
        return format!(
            "No log lines for {} in this window{}.",
            p.slug,
            query.map(|q| format!(" matching '{q}'")).unwrap_or_default()
        );
    }

    let busiest = per_hour
        .iter()
        .max_by_key(|(_, c)| **c)
        .map(|(ts, c)| format!("{} ({c} lines)", fmt::clock(*ts)))
        .unwrap_or_else(|| "-".into());
    let lines: Vec<String> = shown
        .iter()
        .map(|l| {
            format!(
                "{ts}  {lvl:<5}  {res:<22}  {line}",
                ts = fmt::clock(l.ts),
                lvl = crate::errors::level_of(&l.line, &l.stream),
                res = l.container.chars().take(22).collect::<String>(),
                line = l.line.chars().take(160).collect::<String>(),
            )
        })
        .collect();

    format!(
        "{slug} · {window} window{q}{lv}{res}\n\
         {total} lines matched: {info} info, {warn} warn, {error} error. Busiest hour {busiest}.\n\
         Showing {shown} (newest first).\n\n{lines}",
        slug = p.slug,
        window = window.unwrap_or_else(|| "24h".into()),
        q = query.map(|q| format!(" · query '{q}'")).unwrap_or_default(),
        lv = level.map(|l| format!(" · level {l}")).unwrap_or_default(),
        res = resource.map(|r| format!(" · resource {r}")).unwrap_or_default(),
        total = all.len(),
        info = info,
        warn = warn,
        error = error,
        busiest = busiest,
        shown = lines.len(),
        lines = lines.join("\n"),
    )
}

async fn tail_logs(api: Api, p: crate::store::Project, params: Value) -> String {
    let lines_n = arg_usize(&params, "lines", 40, 200);
    let st = api.state.read().await;
    let containers: Vec<crate::metrics::ProjectContainer> = st
        .projects_live
        .get(&p.slug)
        .map(|l| l.containers.clone())
        .unwrap_or_default();
    drop(st);

    let target = match arg_str(&params, "resource") {
        Some(r) => r,
        None => match containers.iter().find(|c| c.role == "app").or_else(|| containers.first()) {
            Some(c) => c.name.clone(),
            None => {
                return format!("{} has no running container to tail.", p.slug);
            }
        },
    };
    if !containers.is_empty() && !containers.iter().any(|c| c.name == target) {
        return format!(
            "'{target}' is not a container of {}. Running: {}",
            p.slug,
            containers.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    let mut lines = crate::logs::tail(&target, lines_n).await;
    if lines.is_empty() {
        return format!("{target} has written nothing yet.");
    }
    lines.reverse(); // newest first, like the index
    let body: Vec<String> = lines
        .iter()
        .map(|l| {
            format!(
                "{ts}  {lvl:<5}  {line}",
                ts = fmt::clock(l.ts),
                lvl = crate::errors::level_of(&l.line, &l.stream),
                line = l.line.chars().take(160).collect::<String>()
            )
        })
        .collect();
    format!(
        "{target} · last {n} lines, live from the container (newest first)\n\n{body}",
        target = target,
        n = body.len(),
        body = body.join("\n")
    )
}

async fn list_errors(api: Api, p: crate::store::Project, params: Value) -> String {
    let state = arg_str(&params, "state").unwrap_or_else(|| "open".into());
    let filter = if state == "all" { None } else { Some(state.as_str()) };
    let issues = api.store.issues(p.id, filter).unwrap_or_default();
    let (open, resolved, ignored) = api.store.issue_counts(p.id).unwrap_or((0, 0, 0));
    let n = now();

    if issues.is_empty() {
        return format!(
            "{} has no {state} issues. Totals: {open} open, {resolved} resolved, {ignored} ignored.",
            p.slug
        );
    }
    let rows: Vec<String> = issues
        .iter()
        .map(|i| {
            format!(
                "#{id}  {count}x  [{src}] {state}\n  {title}\n  {culprit}first {first}, last {last}",
                id = i.id,
                count = i.count,
                src = i.source,
                state = i.state,
                title = i.title.chars().take(150).collect::<String>(),
                culprit = i.culprit.as_ref().map(|c| format!("{c} · ")).unwrap_or_default(),
                first = fmt::ago(i.first_seen, n),
                last = fmt::ago(i.last_seen, n),
            )
        })
        .collect();
    format!(
        "{slug} · {shown} {state} issues (totals: {open} open, {resolved} resolved, {ignored} ignored)\n\
         Grouped by cause; call error_detail with an id for the stack trace.\n\n{rows}",
        slug = p.slug,
        shown = rows.len(),
        state = state,
        rows = rows.join("\n\n"),
    )
}

async fn error_detail(api: Api, p: crate::store::Project, params: Value) -> String {
    let Some(id) = params
        .get("arguments")
        .and_then(|a| a.get("issue_id"))
        .and_then(|v| v.as_i64())
    else {
        return "error_detail needs issue_id — list_errors shows the ids.".into();
    };
    let all = api.store.issues(p.id, None).unwrap_or_default();
    let Some(issue) = all.iter().find(|i| i.id == id) else {
        return format!(
            "{} has no issue #{id}. Open ids: {}",
            p.slug,
            all.iter().map(|i| i.id.to_string()).collect::<Vec<_>>().join(", ")
        );
    };
    let events = api.store.issue_events(id, 10).unwrap_or_default();
    let n = now();
    let occurrences: Vec<String> = events
        .iter()
        .map(|e| {
            format!(
                "{stamp} ({ago}) from {origin}\n{body}",
                stamp = fmt::clock(e.ts),
                ago = fmt::ago(e.ts, n),
                origin = if e.origin.is_empty() { "unknown".into() } else { e.origin.clone() },
                body = fmt::indent(&fmt::cap(&e.message, 1400), "  "),
            )
        })
        .collect();
    format!(
        "Issue #{id} · {state} · {count} occurrences · source {src}\n\
         {title}\n\
         {culprit}first seen {first}, last {last}\n\
         \n\
         Latest {shown} occurrences:\n\n{body}",
        id = issue.id,
        state = issue.state,
        count = issue.count,
        src = issue.source,
        title = issue.title,
        culprit = issue
            .culprit
            .as_ref()
            .map(|c| format!("blamed file: {c}\n"))
            .unwrap_or_default(),
        first = fmt::ago(issue.first_seen, n),
        last = fmt::ago(issue.last_seen, n),
        shown = occurrences.len(),
        body = occurrences.join("\n\n"),
    )
}

// ---------------------------------------------------------------- resources

/// The knowledge that until now only lived in session memory.
const RUNBOOK: &str = include_str!("../RUNBOOK.md");

async fn read_resource(api: &Api, params: &Value) -> Result<Value, String> {
    let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    let body = match uri {
        "webo://runbook" => RUNBOOK.to_string(),
        "webo://projects" => list_projects(api).await,
        other => return Err(format!("unknown resource: {other}")),
    };
    let mime = if uri.ends_with("runbook") { "text/markdown" } else { "text/plain" };
    Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": body }] }))
}

// ---------------------------------------------------------------- prompts

fn get_prompt(params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name != "diagnose_project" {
        return Err(format!("unknown prompt: {name}"));
    }
    let slug = params
        .get("arguments")
        .and_then(|a| a.get("slug"))
        .and_then(|v| v.as_str())
        .unwrap_or("the project");
    let body = format!(
        "Work out what is wrong with {slug} on this server. Follow this order — it is the one \
         that avoids dead ends:\n\
         \n\
         1. project_status({slug}) — is it running? how many restarts? did the last deploy pass?\n\
         2. If there are open errors: list_errors({slug}), then error_detail on the most frequent \
            one. The blamed file and the stack trace usually name the bug outright.\n\
         3. search_logs({slug}) around the time the error started — the busiest hour in the answer \
            tells you the window to look at. Filter by level=error first, then widen.\n\
         4. project_metrics({slug}) — did CPU or memory peak at the same time? A restart with a \
            memory peak just before it is an OOM, not a code bug.\n\
         5. Read webo://runbook before proposing any change to the server itself.\n\
         \n\
         Finish with: what is broken, the evidence for it, and the smallest change that would fix \
         it. If the evidence does not support a conclusion, say what is missing instead of guessing."
    );
    Ok(json!({
        "description": format!("Diagnose {slug}"),
        "messages": [{ "role": "user", "content": { "type": "text", "text": body } }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    async fn rpc_call(api: Api, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let res = app(api)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(res.into_body(), 1 << 22).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The text a tool answered with, so tests can assert on what a model reads.
    async fn tool_text(api: Api, name: &str, args: Value) -> String {
        let out = rpc_call(api, "tools/call", json!({ "name": name, "arguments": args })).await;
        assert!(out.get("error").is_none(), "tool errored: {out}");
        out["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    #[tokio::test]
    async fn handshake_advertises_the_three_surfaces() {
        let api = crate::server::tests::api_with_data();
        let out = rpc_call(api, "initialize", json!({})).await;
        let r = &out["result"];
        assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["serverInfo"]["name"], "webo");
        assert!(r["capabilities"]["tools"].is_object());
        assert!(r["capabilities"]["resources"].is_object());
        assert!(r["capabilities"]["prompts"].is_object());
        assert!(
            r["instructions"].as_str().unwrap().contains("read-only"),
            "the agent is told what it may do"
        );
    }

    #[tokio::test]
    async fn the_catalog_is_nine_read_only_tools_with_usable_schemas() {
        let out = rpc_call(crate::server::tests::api_with_data(), "tools/list", json!({})).await;
        let tools = out["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 9, "phase 1 is nine tools");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in [
            "server_health", "server_processes", "list_projects", "project_status",
            "project_metrics", "search_logs", "tail_logs", "list_errors", "error_detail",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        for t in tools {
            let name = t["name"].as_str().unwrap();
            assert_eq!(t["annotations"]["readOnlyHint"], true, "{name} must be read-only in phase 1");
            let desc = t["description"].as_str().unwrap();
            assert!(desc.len() > 60, "{name} needs a description an agent can choose by");
            assert_eq!(t["inputSchema"]["type"], "object", "{name}");
            // every project tool requires the slug, and says where to get it
            if name.starts_with("project_") || name.ends_with("_logs") || name.contains("error") {
                let required = t["inputSchema"]["required"].as_array().unwrap();
                assert!(
                    required.iter().any(|r| r == "slug"),
                    "{name} should require a slug"
                );
            }
        }
    }

    #[tokio::test]
    async fn server_health_reads_as_an_answer_not_as_json() {
        let text = tool_text(crate::server::tests::api_with_data(), "server_health", json!({})).await;
        // formatted values, not raw numbers
        assert!(text.contains("test-host"), "{text}");
        assert!(text.contains("CPU"), "{text}");
        assert!(text.contains("Memory"), "{text}");
        assert!(text.contains("Projects"), "{text}");
        assert!(text.contains("Docker"), "{text}");
        assert!(!text.contains("mem_used"), "no field names leak: {text}");
        assert!(!text.contains('{'), "not json: {text}");
    }

    #[tokio::test]
    async fn list_projects_puts_what_needs_attention_first() {
        let api = crate::server::tests::api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        // cloudflared is listed before codo alphabetically; give codo an error
        api.store
            .record_error(id, "fp", "boom", "server", "codo", "boom", 10, None)
            .unwrap();
        let text = tool_text(api, "list_projects", json!({})).await;
        let codo_at = text.find("codo").unwrap();
        let cf_at = text.find("cloudflared").unwrap();
        assert!(codo_at < cf_at, "a project with errors comes first:\n{text}");
        assert!(text.contains("1 open errors"), "{text}");
        assert!(text.contains("codo.example.com"), "the domain is there: {text}");
    }

    #[tokio::test]
    async fn project_status_answers_without_a_follow_up() {
        let api = crate::server::tests::api_with_data();
        let text = tool_text(api, "project_status", json!({ "slug": "codo" })).await;
        for expected in ["Resources", "Domains", "Database", "Recent deploys", "Errors"] {
            assert!(text.contains(expected), "missing section {expected}:\n{text}");
        }
        assert!(text.contains("murichristopher/codo"), "{text}");
        assert!(text.contains("210.0 MB"), "container memory is formatted: {text}");
        assert!(text.contains("4f44710"), "the running version is named: {text}");
        assert!(text.contains("feat: nice"), "the last deploy is described: {text}");
    }

    #[tokio::test]
    async fn a_wrong_slug_answers_with_the_ones_that_exist() {
        let out = rpc_call(
            crate::server::tests::api_with_data(),
            "tools/call",
            json!({ "name": "project_status", "arguments": { "slug": "nope" } }),
        )
        .await;
        let msg = out["error"]["message"].as_str().unwrap();
        assert!(msg.contains("no project named 'nope'"), "{msg}");
        assert!(msg.contains("codo"), "it lists what does exist: {msg}");
    }

    #[tokio::test]
    async fn metrics_are_summarised_never_dumped() {
        let api = crate::server::tests::api_with_data();
        // 400 samples: the answer must not carry them
        {
            let mut st = api.state.write().await;
            let live = st.projects_live.get_mut("codo").unwrap();
            live.history.clear();
            let base = now() as u64 - 400 * 15;
            for i in 0..400u64 {
                live.history.push_back(crate::metrics::ProjectSample {
                    ts: base + i * 15,
                    cpu_pct: 0.1 + (i as f32 / 400.0),
                    mem_bytes: 200_000_000 + i * 100_000,
                    disk_bps: 0,
                });
            }
        }
        let text = tool_text(api, "project_metrics", json!({ "slug": "codo", "window": "24h" })).await;
        assert!(text.contains("400 samples"), "the total is stated: {text}");
        assert!(text.contains("rising"), "the trend is named: {text}");
        assert!(text.contains("peak"), "{text}");
        assert!(text.contains("ago)"), "the peak carries when it happened: {text}");
        assert!(text.contains("12 of 400 points"), "only a sample is shown: {text}");
        assert!(text.lines().count() < 30, "the answer stays small: {} lines", text.lines().count());
    }

    #[tokio::test]
    async fn logs_carry_counts_the_busiest_hour_and_respect_the_level() {
        let api = crate::server::tests::api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        let base = now() - 1800;
        let mk = |ts: i64, stream: &str, line: &str| crate::store::LogLine {
            ts, container: "codo".into(), stream: stream.into(), line: line.into(),
        };
        api.store
            .insert_logs(id, &[
                mk(base, "stdout", "GET /health 200"),
                mk(base + 10, "stdout", "WARN cache miss on leads"),
                mk(base + 20, "stderr", "ERROR: connection refused to postgres"),
            ])
            .unwrap();

        let text = tool_text(api.clone(), "search_logs", json!({ "slug": "codo" })).await;
        assert!(text.contains("3 lines matched"), "{text}");
        assert!(text.contains("1 info, 1 warn, 1 error"), "{text}");
        assert!(text.contains("Busiest hour"), "{text}");
        assert!(text.contains("connection refused"), "{text}");

        let only_errors =
            tool_text(api.clone(), "search_logs", json!({ "slug": "codo", "level": "error" })).await;
        assert!(only_errors.contains("connection refused"), "{only_errors}");
        assert!(!only_errors.contains("GET /health"), "the level filter narrows: {only_errors}");
        assert!(only_errors.contains("3 lines matched"), "but the counts stay whole: {only_errors}");

        let by_query =
            tool_text(api, "search_logs", json!({ "slug": "codo", "query": "postgres" })).await;
        assert!(by_query.contains("connection refused"), "{by_query}");
        assert!(by_query.contains("1 lines matched"), "full text narrows the set: {by_query}");
    }

    #[tokio::test]
    async fn errors_list_and_detail_carry_the_stack() {
        let api = crate::server::tests::api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        let issue = api
            .store
            .record_error(
                id,
                "fp-a",
                "NoMethodError (undefined method `valor' for nil)",
                "server",
                "codo",
                "NoMethodError (undefined method `valor' for nil)\n    at boom (app/x.rb:31:5)",
                now() - 60,
                Some("app/x.rb:31:5"),
            )
            .unwrap();

        let list = tool_text(api.clone(), "list_errors", json!({ "slug": "codo" })).await;
        assert!(list.contains(&format!("#{issue}")), "ids are shown so detail can be called: {list}");
        assert!(list.contains("NoMethodError"), "{list}");
        assert!(list.contains("app/x.rb:31:5"), "the blamed file is in the list: {list}");
        assert!(list.contains("1 open"), "{list}");

        let detail =
            tool_text(api.clone(), "error_detail", json!({ "slug": "codo", "issue_id": issue })).await;
        assert!(detail.contains("blamed file: app/x.rb:31:5"), "{detail}");
        assert!(detail.contains("at boom (app/x.rb:31:5)"), "the stack travels: {detail}");
        assert!(detail.contains("ago) from codo"), "the origin is named: {detail}");

        // an unknown id says which ones exist
        let missing =
            tool_text(api, "error_detail", json!({ "slug": "codo", "issue_id": 9999 })).await;
        assert!(missing.contains("no issue #9999"), "{missing}");
    }

    #[tokio::test]
    async fn processes_can_be_filtered_and_sorted() {
        let api = crate::server::tests::api_with_data();
        let text = tool_text(api.clone(), "server_processes", json!({})).await;
        assert!(text.contains("codo"), "{text}");
        assert!(text.contains("sorted by cpu"), "{text}");
        assert!(text.contains("Host totals"), "{text}");

        let filtered =
            tool_text(api, "server_processes", json!({ "filter": "nothing-matches-this" })).await;
        assert!(filtered.contains("No process group matches"), "{filtered}");
    }

    #[tokio::test]
    async fn resources_carry_the_runbook_and_the_inventory() {
        let api = crate::server::tests::api_with_data();
        let list = rpc_call(api.clone(), "resources/list", json!({})).await;
        let uris: Vec<&str> = list["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"webo://runbook"));
        assert!(uris.contains(&"webo://projects"));

        let runbook = rpc_call(api.clone(), "resources/read", json!({ "uri": "webo://runbook" })).await;
        let body = runbook["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(body.len() > 400, "the runbook has content");
        assert!(body.contains("cfargotunnel") || body.contains("tunnel"), "it covers the tunnel");
        assert_eq!(runbook["result"]["contents"][0]["mimeType"], "text/markdown");

        let projects = rpc_call(api.clone(), "resources/read", json!({ "uri": "webo://projects" })).await;
        assert!(projects["result"]["contents"][0]["text"].as_str().unwrap().contains("codo"));

        let unknown = rpc_call(api, "resources/read", json!({ "uri": "webo://nope" })).await;
        assert!(unknown["error"]["message"].as_str().unwrap().contains("unknown resource"));
    }

    #[tokio::test]
    async fn the_prompt_is_an_order_of_operations() {
        let api = crate::server::tests::api_with_data();
        let list = rpc_call(api.clone(), "prompts/list", json!({})).await;
        assert_eq!(list["result"]["prompts"][0]["name"], "diagnose_project");

        let got = rpc_call(
            api,
            "prompts/get",
            json!({ "name": "diagnose_project", "arguments": { "slug": "codo" } }),
        )
        .await;
        let body = got["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(body.contains("project_status(codo)"), "{body}");
        // the order matters: status before errors before logs before metrics
        let at = |needle: &str| body.find(needle).unwrap_or(usize::MAX);
        assert!(at("project_status") < at("list_errors"), "status first");
        assert!(at("list_errors") < at("search_logs"), "errors before logs");
        assert!(at("search_logs") < at("project_metrics"), "logs before metrics");
        assert!(body.contains("webo://runbook"), "it points at the runbook: {body}");
        assert!(body.contains("say what is missing"), "it forbids guessing: {body}");
    }

    #[tokio::test]
    async fn unknown_methods_and_tools_fail_cleanly() {
        let api = crate::server::tests::api_with_data();
        let bad_method = rpc_call(api.clone(), "does/not/exist", json!({})).await;
        assert!(bad_method["error"]["message"].as_str().unwrap().contains("unknown method"));

        let bad_tool = rpc_call(api.clone(), "tools/call", json!({ "name": "rm_rf" })).await;
        assert!(bad_tool["error"]["message"].as_str().unwrap().contains("unknown tool"));

        // a tool that needs a slug says so instead of panicking
        let no_slug = rpc_call(api.clone(), "tools/call", json!({ "name": "project_status" })).await;
        assert!(no_slug["error"]["message"].as_str().unwrap().contains("needs a slug"));

        // ping is answered, and a notification (no id) is silently accepted
        assert!(rpc_call(api.clone(), "ping", json!({})).await["result"].is_object());
        let notif = app(api)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(notif.status(), axum::http::StatusCode::OK);
    }
}
