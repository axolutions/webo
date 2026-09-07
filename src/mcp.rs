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
trace; search_logs finds the lines around it. db_info shows a project's schema \
before db_rows or db_query read from it. Tools that change something say so in \
their annotations and need an explicit argument — nothing here writes by \
accident, and every write is recorded in webo's own logs. Variable values are \
always masked. Read webo://runbook before suggesting any change to the server.";

/// Every call that changes something is written to stdout, which webo's own
/// log collector indexes like any other container — so what the agent did is
/// visible in the panel, next to everything else, and survives a restart.
/// Without this there is no way to find out afterwards.
fn audit(tool: &str, params: &Value, outcome: &str) {
    let args = params
        .get("arguments")
        .map(|a| {
            // arguments are small by design; a SQL statement is the exception
            let text = a.to_string();
            if text.len() > 400 { format!("{}…", &text[..400]) } else { text }
        })
        .unwrap_or_else(|| "{}".into());
    println!("[webo-mcp] {tool} {args} -> {outcome}");
}

/// The tools that change something. Kept as one list so audit and the
/// annotations cannot drift apart.
const WRITE_TOOLS: [&str; 8] = [
    "db_query", "db_backup", "triage_errors",
    "create_project", "deploy_project", "project_env", "connect_domain", "delete_project",
];

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

/// A tool that changes something. The annotations are honest so a client can
/// ask the user before calling it — `destructive` marks the ones that can lose
/// data even when the arguments are right.
fn write_tool(name: &str, description: &str, input: Value, destructive: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input,
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": destructive,
            "idempotentHint": false,
            "openWorldHint": false,
        },
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
            "db_info",
            "What database a project has (its own Postgres container, or a SQLite file found in a \
             volume) and every table with an estimated row count. The map to read before querying.",
            schema(json!({ "slug": { "type": "string", "description": SLUG_DESC } }), vec!["slug"]),
        ),
        tool(
            "db_rows",
            "Browse one table with paging and ordering, without writing SQL. Cheaper and safer than \
             db_query for the common case of looking at the data.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "table": { "type": "string", "description": "Table name, as listed by db_info." },
                    "limit": { "type": "integer", "description": "Rows per page (default 20, max 100)." },
                    "offset": { "type": "integer", "description": "Rows to skip." },
                    "order_by": { "type": "string", "description": "Column to sort by." },
                    "descending": { "type": "boolean", "description": "Sort descending instead of ascending." }
                }),
                vec!["slug", "table"],
            ),
        ),
        write_tool(
            "db_query",
            "Run SQL. READ-ONLY unless write:true — the Postgres session itself is opened read-only, \
             so a stray UPDATE is refused by the database, not by a check here. Setting write:true \
             takes a backup first and names the file in the answer, so the change is reversible.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "sql": { "type": "string", "description": "The statement to run." },
                    "write": { "type": "boolean", "description": "Allow statements that change data. A backup is taken first." }
                }),
                vec!["slug", "sql"],
            ),
            false,
        ),
        write_tool(
            "db_backup",
            "List a project's database dumps, take one now, or restore one. Restoring overwrites the \
             current data and needs confirm:true. Downloading is not offered — the answer gives the \
             file name and size instead.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "action": { "type": "string", "enum": ["list", "create", "restore"], "description": "Defaults to list." },
                    "file": { "type": "string", "description": "Which dump to restore, from the list." },
                    "confirm": { "type": "boolean", "description": "Required to restore: it overwrites current data." }
                }),
                vec!["slug"],
            ),
            true,
        ),
        write_tool(
            "triage_errors",
            "Resolve, ignore, reopen or delete issues, one or many at once. Ignoring silences an \
             issue even when it happens again; resolving lets it reopen. Deleting drops the \
             occurrences too and needs confirm:true.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "issue_ids": { "type": "array", "items": { "type": "integer" }, "description": "Ids from list_errors." },
                    "action": { "type": "string", "enum": ["resolve", "ignore", "reopen", "delete"], "description": "What to do with them." },
                    "confirm": { "type": "boolean", "description": "Required to delete." }
                }),
                vec!["slug", "issue_ids", "action"],
            ),
            true,
        ),
        write_tool(
            "create_project",
            "Register a GitHub repository as a project: detects the stack (Rails or Next) and the \
             Ruby or Node version, and returns the scaffold plan. It does NOT deploy — call \
             deploy_project after showing the plan.",
            schema(
                json!({
                    "repo_owner": { "type": "string", "description": "GitHub owner or organisation." },
                    "repo_name": { "type": "string", "description": "Repository name." }
                }),
                vec!["repo_owner", "repo_name"],
            ),
            false,
        ),
        write_tool(
            "deploy_project",
            "Run the first deploy of a registered project: creates the database if asked for, writes \
             the variables, commits the scaffold and reserves the automatic domain — which starts \
             the build on GitHub Actions. This is the call that changes the world.",
            schema(
                json!({
                    "slug": { "type": "string", "description": "The project, as returned by create_project." },
                    "database": { "type": "string", "enum": ["managed", "external", "none"], "description": "managed creates a Postgres container; external takes database_url; defaults to none." },
                    "pg_version": { "type": "string", "enum": ["17", "16", "15"], "description": "For a managed database. Defaults to 17." },
                    "database_url": { "type": "string", "description": "Required when database is external." },
                    "env": { "type": "object", "description": "Environment variables to write before the first deploy." }
                }),
                vec!["slug"],
            ),
            false,
        ),
        write_tool(
            "project_env",
            "List, set or remove a project's environment variables. Values are ALWAYS masked — this \
             tool never returns a secret in clear text, by design. Setting one rewrites the app's \
             .env on the server; it takes effect on the next deploy.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "action": { "type": "string", "enum": ["list", "set", "delete"], "description": "Defaults to list." },
                    "key": { "type": "string", "description": "Variable name, for set and delete." },
                    "value": { "type": "string", "description": "The value, for set." }
                }),
                vec!["slug"],
            ),
            false,
        ),
        write_tool(
            "connect_domain",
            "Connect or disconnect a custom domain. Inside webo's own Cloudflare zone the DNS record \
             is created automatically; elsewhere the answer says which CNAME to point. The automatic \
             domain is never touched.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "action": { "type": "string", "enum": ["connect", "disconnect"], "description": "Defaults to connect." },
                    "domain": { "type": "string", "description": "Hostname to connect, e.g. app.example.com." }
                }),
                vec!["slug"],
            ),
            false,
        ),
        write_tool(
            "delete_project",
            "Stop and remove a project's containers. Needs confirm to equal the slug exactly. Volumes \
             and images are NEVER removed through MCP — deleting data is a panel-only action, where \
             a person is looking. The GitHub repository is untouched.",
            schema(
                json!({
                    "slug": { "type": "string", "description": SLUG_DESC },
                    "confirm": { "type": "string", "description": "Must equal the slug, exactly as the panel makes you type it." }
                }),
                vec!["slug", "confirm"],
            ),
            true,
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
    vec![
        json!({
            "name": "diagnose_project",
            "description": "Work out why a project is failing or slow, in the order that avoids dead ends.",
            "arguments": [{ "name": "slug", "description": SLUG_DESC, "required": true }],
        }),
        json!({
            "name": "why_did_deploy_fail",
            "description": "Find out why a deploy did not go up, checking the failure modes that have actually happened on this server.",
            "arguments": [{ "name": "slug", "description": SLUG_DESC, "required": true }],
        }),
        json!({
            "name": "explore_data",
            "description": "Answer a question from a project's database, reading the schema before writing any SQL.",
            "arguments": [
                { "name": "slug", "description": SLUG_DESC, "required": true },
                { "name": "question", "description": "What you want to know from the data.", "required": false },
            ],
        }),
    ]
}

// ---------------------------------------------------------------- tools

async fn call_tool(api: &Api, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let out = dispatch(api, name, params).await;
    if WRITE_TOOLS.contains(&name) {
        // the first line of the answer says what happened, and a refusal is as
        // worth recording as a change
        let outcome = match &out {
            Ok(v) => v["content"][0]["text"]
                .as_str()
                .and_then(|t| t.lines().next())
                .unwrap_or("done")
                .chars()
                .take(160)
                .collect::<String>(),
            Err(e) => format!("REFUSED: {e}"),
        };
        audit(name, params, &outcome);
    }
    out
}

async fn dispatch(api: &Api, name: &str, params: &Value) -> Result<Value, String> {
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
        "db_info" => run(api, params, db_info).await,
        "db_rows" => run(api, params, db_rows).await,
        "db_query" => run(api, params, db_query).await,
        "db_backup" => run(api, params, db_backup).await,
        "triage_errors" => run(api, params, triage_errors).await,
        "create_project" => create_project(api, params).await,
        "deploy_project" => run(api, params, deploy_project).await,
        "project_env" => run(api, params, project_env).await,
        "connect_domain" => run(api, params, connect_domain).await,
        "delete_project" => run(api, params, delete_project).await,
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

// ---------------------------------------------------------------- database

async fn db_info(api: Api, p: crate::store::Project, _params: Value) -> String {
    let Some(db) = api.store.database(p.id).ok().flatten() else {
        return format!("{} has no database yet.", p.slug);
    };
    let head = if db.kind == "postgres" {
        format!(
            "{} · postgres in its own container ({}), database {}",
            p.slug,
            db.container.clone().unwrap_or_default(),
            db.db_name.clone().unwrap_or_default()
        )
    } else {
        format!(
            "{} · sqlite at {} in volume {}{}",
            p.slug,
            db.file_path.clone().unwrap_or_default(),
            db.volume.clone().unwrap_or_default(),
            if db.persisted { "" } else { "\nWARNING: this file sits in the container layer — every deploy wipes it" }
        )
    };
    let (_, tables) = match crate::server::table_names(&api, &p.slug).await {
        Ok(t) => t,
        Err((_, msg)) => return format!("{head}\n\nCould not read the schema: {msg}"),
    };
    if tables.is_empty() {
        return format!("{head}\n\nNo tables yet.");
    }
    // one pass for estimated counts; exact counts would scan every table
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if db.kind == "postgres" {
        if let Ok(out) = crate::server::run_sql(
            &api, &p.slug, "SELECT relname, n_live_tup FROM pg_stat_user_tables", false,
        ).await {
            for row in crate::db::parse_table_output(&out, 500).rows {
                if let (Some(name), Some(n)) = (row.first(), row.get(1)) {
                    counts.insert(name.clone(), n.parse().unwrap_or(0));
                }
            }
        }
    }
    let rows: Vec<String> = tables
        .iter()
        .map(|t| match counts.get(t) {
            Some(n) => format!("  {t:<28} ~{n} rows"),
            None => format!("  {t}"),
        })
        .collect();
    format!(
        "{head}\n\n{n} tables{est}:\n{rows}\n\nUse db_rows to look at one, or db_query for anything else.",
        n = tables.len(),
        est = if counts.is_empty() { "" } else { " (counts are estimates)" },
        rows = rows.join("\n"),
    )
}

async fn db_rows(api: Api, p: crate::store::Project, params: Value) -> String {
    let Some(table) = arg_str(&params, "table") else {
        return "db_rows needs a table — db_info lists them.".into();
    };
    let limit = arg_usize(&params, "limit", 20, 100);
    let offset = params.get("arguments").and_then(|a| a.get("offset")).and_then(|v| v.as_u64()).unwrap_or(0);

    let (_, tables) = match crate::server::table_names(&api, &p.slug).await {
        Ok(t) => t,
        Err((_, msg)) => return msg,
    };
    // the name reaches SQL only after it matched a real table
    if !tables.contains(&table) {
        return format!("{} has no table '{table}'. Tables: {}", p.slug, tables.join(", "));
    }
    let order = match arg_str(&params, "order_by") {
        Some(col) if crate::server::ident_ok(&col) => {
            let desc = params.get("arguments").and_then(|a| a.get("descending")).and_then(|v| v.as_bool()).unwrap_or(false);
            format!(" ORDER BY \"{col}\" {}", if desc { "DESC" } else { "ASC" })
        }
        Some(bad) => return format!("'{bad}' is not a valid column name."),
        None => String::new(),
    };
    let sql = format!("SELECT * FROM \"{table}\"{order} LIMIT {limit} OFFSET {offset}");
    let out = match crate::server::run_sql(&api, &p.slug, &sql, false).await {
        Ok(o) => o,
        Err((_, msg)) => return format!("Query failed: {msg}"),
    };
    let parsed = crate::db::parse_table_output(&out, limit);
    if parsed.rows.is_empty() {
        return format!("{table} has no rows at offset {offset}.");
    }
    let total = crate::server::run_sql(&api, &p.slug, &format!("SELECT COUNT(*) FROM \"{table}\""), false)
        .await
        .ok()
        .and_then(|o| crate::db::parse_table_output(&o, 2).rows.first().and_then(|r| r.first()).and_then(|v| v.parse::<i64>().ok()))
        .unwrap_or(-1);

    // align the columns so the model can read the table as a table
    let widths: Vec<usize> = parsed.columns.iter().enumerate().map(|(i, c)| {
        parsed.rows.iter().filter_map(|r| r.get(i).map(|v| v.chars().count()))
            .chain(std::iter::once(c.chars().count())).max().unwrap_or(8).min(38)
    }).collect();
    let cell = |v: &str, w: usize| { let t: String = v.chars().take(w).collect(); format!("{t:<w$}", w = w) };
    let header = parsed.columns.iter().enumerate().map(|(i, c)| cell(c, widths[i])).collect::<Vec<_>>().join("  ");
    let body: Vec<String> = parsed.rows.iter().map(|r| {
        r.iter().enumerate().map(|(i, v)| cell(v, *widths.get(i).unwrap_or(&20))).collect::<Vec<_>>().join("  ")
    }).collect();
    format!(
        "{table} · rows {from}–{to}{of}{order_note}\n\n{header}\n{rule}\n{body}",
        from = offset + 1,
        to = offset + parsed.rows.len() as u64,
        of = if total >= 0 { format!(" of {total}") } else { String::new() },
        order_note = arg_str(&params, "order_by").map(|c| format!(" · ordered by {c}")).unwrap_or_default(),
        header = header,
        rule = "-".repeat(header.chars().count().min(120)),
        body = body.join("\n"),
    )
}

async fn db_query(api: Api, p: crate::store::Project, params: Value) -> String {
    let Some(sql) = arg_str(&params, "sql") else { return "db_query needs sql.".into() };
    let write = params.get("arguments").and_then(|a| a.get("write")).and_then(|v| v.as_bool()).unwrap_or(false);
    let changes = crate::db::is_write_statement(&sql);
    if changes && !write {
        return format!(
            "This statement changes data, and write was not set:\n  {}\n\nCall again with \
             write:true if that is what you mean. A backup is taken first.",
            sql.lines().next().unwrap_or("").chars().take(120).collect::<String>()
        );
    }

    // a write is only reversible if a dump exists from before it
    let mut prelude = String::new();
    if changes {
        match api.store.database(p.id).ok().flatten() {
            Some(d) if d.kind == "postgres" => {
                match crate::backups::dump(&d, &crate::server::app_network(), &p.slug).await {
                    Ok(file) => prelude = format!("Backed up to {file} before running this.\n\n"),
                    Err(e) => return format!(
                        "Refusing to write: the backup failed, so the change would not be reversible.\n{e}"
                    ),
                }
            }
            Some(_) => {
                prelude = "NOTE: this is SQLite and webo does not back those up — this change \
                           cannot be rolled back.\n\n".into()
            }
            None => return format!("{} has no database.", p.slug),
        }
    }

    let out = match crate::server::run_sql(&api, &p.slug, &sql, write).await {
        Ok(o) => o,
        Err((_, msg)) => return format!("{prelude}Query failed: {msg}"),
    };
    if out.to_lowercase().contains("error") {
        return format!("{prelude}The database refused it:\n{}", fmt::cap(out.trim(), 1200));
    }
    let parsed = crate::db::parse_table_output(&out, 60);
    if parsed.columns.is_empty() {
        return format!("{prelude}Done. The database returned no rows.\n{}", fmt::cap(out.trim(), 400));
    }
    let header = parsed.columns.join(" | ");
    let body: Vec<String> = parsed.rows.iter().map(|r| r.join(" | ")).collect();
    format!(
        "{prelude}{n} rows{trunc}\n\n{header}\n{rule}\n{body}",
        n = parsed.row_count,
        trunc = if parsed.truncated { " (showing the first ones)" } else { "" },
        header = header,
        rule = "-".repeat(header.chars().count().min(120)),
        body = body.join("\n"),
    )
}

async fn db_backup(api: Api, p: crate::store::Project, params: Value) -> String {
    let action = arg_str(&params, "action").unwrap_or_else(|| "list".into());
    let Some(db) = api.store.database(p.id).ok().flatten() else {
        return format!("{} has no database.", p.slug);
    };
    if db.kind != "postgres" {
        return format!(
            "{} uses SQLite, and webo only backs up Postgres. The file lives in a volume; copying \
             it is a server-side job.",
            p.slug
        );
    }
    let root = std::path::PathBuf::from(crate::backups::backups_root());
    let n = now();
    match action.as_str() {
        "create" => match crate::backups::dump(&db, &crate::server::app_network(), &p.slug).await {
            Ok(file) => {
                crate::backups::prune(&root, &p.slug, crate::backups::KEEP_PER_PROJECT);
                let size = crate::backups::list(&root, &p.slug).into_iter()
                    .find(|b| b.file == file).map(|b| fmt::bytes(b.size_bytes))
                    .unwrap_or_else(|| "unknown size".into());
                format!("Backed up {} to {file} ({size}).", p.slug)
            }
            Err(e) => format!("Backup failed: {e}"),
        },
        "restore" => {
            let Some(file) = arg_str(&params, "file") else {
                return "restore needs the file name — call with action:list to see them.".into();
            };
            let confirmed = params.get("arguments").and_then(|a| a.get("confirm")).and_then(|v| v.as_bool()).unwrap_or(false);
            if !confirmed {
                return format!(
                    "Restoring {file} would overwrite everything in {}'s database with the contents \
                     of that dump. Call again with confirm:true if that is what you want.",
                    p.slug
                );
            }
            match crate::backups::restore(&db, &crate::server::app_network(), &p.slug, &file).await {
                Ok(()) => format!("Restored {} from {file}.", p.slug),
                Err(e) => format!("Restore failed: {e}"),
            }
        }
        _ => {
            let files = crate::backups::list(&root, &p.slug);
            if files.is_empty() {
                return format!(
                    "{} has no backups yet. One is taken daily; call with action:create to make one now.",
                    p.slug
                );
            }
            let rows: Vec<String> = files.iter()
                .map(|b| format!("  {}  {:>9}  {}", b.file, fmt::bytes(b.size_bytes), fmt::ago(b.created_at, n)))
                .collect();
            format!(
                "{slug} · {n} backups (daily, {keep} kept)\n\n{rows}\n\nRestore with action:restore, \
                 file:<name> and confirm:true.",
                slug = p.slug, n = files.len(), keep = crate::backups::KEEP_PER_PROJECT,
                rows = rows.join("\n"),
            )
        }
    }
}

async fn triage_errors(api: Api, p: crate::store::Project, params: Value) -> String {
    let ids: Vec<i64> = params.get("arguments").and_then(|a| a.get("issue_ids")).and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect()).unwrap_or_default();
    if ids.is_empty() {
        return "triage_errors needs issue_ids — list_errors shows them.".into();
    }
    let Some(action) = arg_str(&params, "action") else {
        return "triage_errors needs an action: resolve, ignore, reopen or delete.".into();
    };
    let known = api.store.issues(p.id, None).unwrap_or_default();
    let unknown: Vec<String> = ids.iter().filter(|id| !known.iter().any(|i| i.id == **id))
        .map(|id| id.to_string()).collect();
    if !unknown.is_empty() {
        return format!(
            "{} has no issue(s) #{}. Existing ids: {}",
            p.slug, unknown.join(", #"),
            known.iter().map(|i| i.id.to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    let changed = match action.as_str() {
        "delete" => {
            let confirmed = params.get("arguments").and_then(|a| a.get("confirm")).and_then(|v| v.as_bool()).unwrap_or(false);
            if !confirmed {
                return format!(
                    "Deleting {} issue(s) also drops their occurrences and stack traces. Call again \
                     with confirm:true. (Resolving instead keeps the history and lets the issue \
                     reopen if it happens again.)",
                    ids.len()
                );
            }
            api.store.delete_issues(p.id, &ids).unwrap_or(0)
        }
        "resolve" => api.store.set_issue_state(p.id, &ids, "resolved").unwrap_or(0),
        "ignore" => api.store.set_issue_state(p.id, &ids, "ignored").unwrap_or(0),
        "reopen" => api.store.set_issue_state(p.id, &ids, "open").unwrap_or(0),
        other => return format!("'{other}' is not an action. Use resolve, ignore, reopen or delete."),
    };
    let (open, resolved, ignored) = api.store.issue_counts(p.id).unwrap_or((0, 0, 0));
    let note = match action.as_str() {
        "ignore" => " Ignored issues stay ignored even when the error happens again.",
        "resolve" => " A resolved issue reopens by itself if the error comes back.",
        _ => "",
    };
    format!(
        "{action}d {changed} issue(s) on {slug}.{note}\nNow: {open} open, {resolved} resolved, {ignored} ignored.",
        action = action.trim_end_matches('e'), changed = changed, slug = p.slug, note = note,
        open = open, resolved = resolved, ignored = ignored,
    )
}

// ---------------------------------------------------------------- operating

/// create_project takes a repo, not a slug, so it does not go through `run`.
async fn create_project(api: &Api, params: &Value) -> Result<Value, String> {
    let (Some(owner), Some(name)) = (arg_str(params, "repo_owner"), arg_str(params, "repo_name")) else {
        return Err("create_project needs repo_owner and repo_name".into());
    };
    let req = crate::server::CreateProject { repo_owner: owner.clone(), repo_name: name.clone() };
    match crate::server::do_create_project(api, req).await {
        Err((_, msg)) => Err(msg),
        Ok(v) if v["supported"] == false => Ok(text(format!(
            "{owner}/{name} is {lang} — webo has no template for it yet, so it cannot be deployed \
             from here. Rails and Next.js are supported today.",
            lang = v["language"].as_str().unwrap_or("an unsupported stack")
        ))),
        Ok(v) => {
            let files: Vec<String> = v["files"]
                .as_array()
                .map(|a| a.iter().filter_map(|f| f.as_str().map(|s| format!("  {s}"))).collect())
                .unwrap_or_default();
            let secrets: Vec<String> = v["secrets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|s| {
                            format!(
                                "  {} {}",
                                s["name"].as_str().unwrap_or(""),
                                if s["configured"] == true { "configured" } else { "MISSING on this server" }
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(text(format!(
                "Registered {owner}/{name} as '{slug}'.\n\n\
                 stack       {tech} (template: {tpl}){ruby}\n\
                 branch      {branch}\n\
                 Dockerfile  {docker}\n\
                 \n\
                 These files will be committed on deploy:\n{files}\n\
                 \n\
                 Deploy secrets:\n{secrets}\n\
                 \n\
                 Nothing has been deployed. Call deploy_project with slug '{slug}' when the plan \
                 above looks right — that is what commits and starts the build.",
                slug = v["slug"].as_str().unwrap_or(""),
                tech = v["tech"].as_str().unwrap_or(""),
                tpl = v["template"].as_str().unwrap_or(""),
                ruby = v["ruby"].as_str().map(|r| format!(" on ruby {r}")).unwrap_or_default(),
                branch = v["branch"].as_str().unwrap_or(""),
                docker = if v["has_dockerfile"] == true { "already in the repo — yours is kept" } else { "will be scaffolded" },
                files = files.join("\n"),
                secrets = secrets.join("\n"),
            )))
        }
    }
}

async fn deploy_project(api: Api, p: crate::store::Project, params: Value) -> String {
    let database = arg_str(&params, "database").unwrap_or_else(|| "none".into());
    let env: std::collections::BTreeMap<String, String> = params
        .get("arguments")
        .and_then(|a| a.get("env"))
        .and_then(|v| v.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let req = crate::server::ProvisionReq {
        db: Some(database.clone()),
        pg_version: arg_str(&params, "pg_version"),
        database_url: arg_str(&params, "database_url"),
        env,
    };
    match crate::server::do_provision(&api, &p.slug, req).await {
        Err((_, msg)) => format!("Deploy did not start: {msg}"),
        Ok(v) => {
            let secrets: Vec<String> = v["secrets"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|s| format!("{} {}", s["name"].as_str().unwrap_or(""), s["status"].as_str().unwrap_or("")))
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "Deploying {slug}.\n\n\
                 commit      {sha}\n\
                 domain      {domain}\n\
                 database    {db}\n\
                 variables   {env}\n\
                 secrets     {secrets}\n\
                 \n\
                 The build is running on GitHub Actions now. Call project_status in a couple of \
                 minutes to see whether it went up.",
                slug = p.slug,
                sha = v["commit_sha"].as_str().unwrap_or("?"),
                domain = v["auto_domain"].as_str().map(|d| format!("https://{d}")).unwrap_or_else(|| "none — Cloudflare is not configured".into()),
                db = match database.as_str() {
                    "managed" => v["database"]["container"].as_str().map(|c| format!("postgres in {c}")).unwrap_or_else(|| "managed".into()),
                    "external" => "external, URL stored".into(),
                    _ => "none".into(),
                },
                env = if v["env_written"] == true { "written to the app's .env" } else { "COULD NOT be written — check the server" },
                secrets = secrets.join(", "),
            )
        }
    }
}

async fn project_env(api: Api, p: crate::store::Project, params: Value) -> String {
    let action = arg_str(&params, "action").unwrap_or_else(|| "list".into());
    match action.as_str() {
        "set" => {
            let (Some(key), Some(value)) = (arg_str(&params, "key"), params.get("arguments").and_then(|a| a.get("value")).and_then(|v| v.as_str()))
            else {
                return "set needs key and value.".into();
            };
            if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return format!("'{key}' is not a valid variable name.");
            }
            if api.store.env_vars(p.id).unwrap_or_default().iter().any(|v| v.key == key && v.managed) {
                return format!(
                    "{key} is managed by webo (it comes with the database) and cannot be set by hand."
                );
            }
            let _ = api.store.set_env(p.id, &key, value, false);
            let written = crate::server::materialize_env(&api, &p.slug).await.is_ok();
            format!(
                "Set {key} on {slug}.{note} It takes effect on the next deploy.",
                slug = p.slug,
                note = if written { " The app's .env was rewritten." } else { " WARNING: the .env could not be written on the server." },
            )
        }
        "delete" => {
            let Some(key) = arg_str(&params, "key") else { return "delete needs a key.".into() };
            if api.store.delete_env(p.id, &key).unwrap_or(false) {
                let _ = crate::server::materialize_env(&api, &p.slug).await;
                format!("Removed {key} from {}.", p.slug)
            } else {
                format!("{key} is either managed by webo or does not exist on {}.", p.slug)
            }
        }
        _ => {
            let vars = api.store.env_vars(p.id).unwrap_or_default();
            let shown: Vec<&crate::store::EnvVar> =
                vars.iter().filter(|v| !v.key.starts_with("__WEBO_")).collect();
            if shown.is_empty() {
                return format!("{} has no variables set.", p.slug);
            }
            let rows: Vec<String> = shown
                .iter()
                .map(|v| {
                    format!(
                        "  {:<28} {}{}",
                        v.key,
                        crate::server::mask(&v.value),
                        if v.managed { "  (managed by webo)" } else { "" }
                    )
                })
                .collect();
            format!(
                "{slug} · {n} variables\n\n{rows}\n\nValues are masked and this tool never returns \
                 them in clear text. Read one in the panel if you must.",
                slug = p.slug,
                n = shown.len(),
                rows = rows.join("\n"),
            )
        }
    }
}

async fn connect_domain(api: Api, p: crate::store::Project, params: Value) -> String {
    let action = arg_str(&params, "action").unwrap_or_else(|| "connect".into());
    if action == "disconnect" {
        if p.custom_domain.is_none() {
            return format!("{} has no custom domain connected.", p.slug);
        }
        return match crate::server::do_disconnect_domain(&api, &p.slug).await {
            Ok(()) => format!(
                "Disconnected {}. The automatic domain{} still works.",
                p.custom_domain.clone().unwrap_or_default(),
                p.auto_domain.map(|a| format!(" ({a})")).unwrap_or_default()
            ),
            Err((_, msg)) => format!("Could not disconnect: {msg}"),
        };
    }
    let Some(domain) = arg_str(&params, "domain") else {
        return "connect needs a domain.".into();
    };
    match crate::server::do_connect_domain(&api, &p.slug, &domain).await {
        Err((_, msg)) => format!("Could not connect {domain}: {msg}"),
        Ok(v) => {
            if v["dns_managed"] == true {
                format!(
                    "Connected {domain} to {slug}. The DNS record was created in webo's zone and is \
                     propagating — it usually answers within a minute.",
                    slug = p.slug
                )
            } else {
                format!(
                    "Connected {domain} to {slug}. It is outside webo's zone, so point a CNAME at \
                     {target} on your own DNS — until then the hostname will not resolve.",
                    slug = p.slug,
                    target = v["cname_target"].as_str().unwrap_or("the tunnel")
                )
            }
        }
    }
}

async fn delete_project(api: Api, p: crate::store::Project, params: Value) -> String {
    if p.slug == "webo" {
        return "webo cannot delete itself.".into();
    }
    let confirm = arg_str(&params, "confirm").unwrap_or_default();
    if confirm != p.slug {
        return format!(
            "To delete {slug}, pass confirm exactly equal to the slug: confirm:\"{slug}\".\n\
             This stops and removes its containers. Volumes (the data) and images are NEVER removed \
             through MCP — that is a panel-only action. The GitHub repository is untouched.",
            slug = p.slug
        );
    }
    // containers only: deleting data is a decision for a person at the panel
    let opts = crate::projects::TeardownOpts { containers: true, volumes: false, images: false };
    let report = crate::server::do_delete_project(&api, &p.slug, opts).await;
    match report {
        Err((_, msg)) => format!("Could not delete: {msg}"),
        Ok(r) => format!(
            "Deleted {slug}: {n} container(s) removed. Its volumes and images are still on the \
             server, and so is the GitHub repository — remove those from the panel if you mean to.",
            slug = p.slug,
            n = r["containers_removed"].as_u64().unwrap_or(0),
        ),
    }
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
    let slug = params
        .get("arguments")
        .and_then(|a| a.get("slug"))
        .and_then(|v| v.as_str())
        .unwrap_or("the project");
    let body = match name {
        "diagnose_project" => diagnose_prompt(slug),
        "why_did_deploy_fail" => deploy_failure_prompt(slug),
        "explore_data" => {
            let question = params
                .get("arguments")
                .and_then(|a| a.get("question"))
                .and_then(|v| v.as_str())
                .unwrap_or("the question you were asked");
            explore_data_prompt(slug, question)
        }
        other => return Err(format!("unknown prompt: {other}")),
    };
    Ok(json!({
        "description": format!("{name} for {slug}"),
        "messages": [{ "role": "user", "content": { "type": "text", "text": body } }],
    }))
}

fn diagnose_prompt(slug: &str) -> String {
    format!(
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
    )
}

/// The failure modes below are the ones that have actually broken a deploy on
/// this server. An agent that checks these first is checking reality, not a
/// generic list.
fn deploy_failure_prompt(slug: &str) -> String {
    format!(
        "Find out why the last deploy of {slug} did not go up. Check in this order — these are \
         the failures that have really happened here:\n\
         \n\
         1. project_status({slug}) — did the build fail, or did it pass and the container still \
            not come up? Those are different problems.\n\
         2. If the build failed, the cause is usually in the repo: a Gemfile pinned to a Ruby the \
            image does not have, or a dependency the Dockerfile never installs.\n\
         3. If the build passed but nothing is running: tail_logs({slug}) — a container that \
            starts and exits leaves its reason in the last lines.\n\
         4. project_env({slug}) — compare what is set against what the app needs. An app booting \
            with no configuration usually means the .env landed in the wrong directory.\n\
         5. list_errors({slug}) — a crash loop shows up here as one issue repeating.\n\
         \n\
         Read webo://runbook: it lists the conventions that, when broken, produce exactly these \
         symptoms. Finish with the cause and the smallest fix, or with what you would need to \
         look at next."
    )
}

fn explore_data_prompt(slug: &str, question: &str) -> String {
    format!(
        "Answer this from {slug}'s database: {question}\n\
         \n\
         Read the schema before writing SQL — inventing a column name is the usual way this goes \
         wrong:\n\
         \n\
         1. db_info({slug}) — the tables and roughly how big they are.\n\
         2. db_rows on the table that looks right — seeing real values tells you what the columns \
            actually hold, which the names often do not.\n\
         3. db_query only when the answer needs aggregation or a join.\n\
         \n\
         Stay read-only. If answering would require changing data, say so and stop — a write needs \
         write:true and takes a backup first, and that is the user's call, not yours."
    )
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
    async fn the_reading_tools_stay_read_only_with_usable_schemas() {
        let out = rpc_call(crate::server::tests::api_with_data(), "tools/list", json!({})).await;
        let tools = out["result"]["tools"].as_array().unwrap();
        const READERS: [&str; 11] = [
            "server_health", "server_processes", "list_projects", "project_status",
            "project_metrics", "search_logs", "tail_logs", "list_errors", "error_detail",
            "db_info", "db_rows",
        ];
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for expected in READERS {
            assert!(names.contains(&expected), "missing {expected}");
        }
        for t in tools {
            let name = t["name"].as_str().unwrap();
            if READERS.contains(&name) {
                assert_eq!(t["annotations"]["readOnlyHint"], true, "{name} must stay read-only");
            }
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

    /// The panel image is built from a Dockerfile that copies only what it
    /// lists. A file pulled in with include_str! that it does not copy compiles
    /// locally and fails the release build — which is exactly how the runbook
    /// broke the first deploy of this module.
    #[test]
    fn everything_embedded_in_the_binary_is_copied_into_the_image() {
        let dockerfile = include_str!("../deploy/Dockerfile");
        let sources = [
            ("src/mcp.rs", include_str!("mcp.rs")),
            ("src/server.rs", include_str!("server.rs")),
            ("src/scaffold.rs", include_str!("scaffold.rs")),
        ];
        for (file, body) in sources {
            // only what ships: everything under #[cfg(test)] is compiled away,
            // and this very test embeds the Dockerfile to read it
            let shipped = body.split("#[cfg(test)]").next().unwrap_or(body);
            for (i, _) in shipped.match_indices("include_str!(\"") {
                let rest = &shipped[i + 14..];
                let path = &rest[..rest.find('"').unwrap()];
                // paths are relative to src/; the Dockerfile copies from the root
                let top = path.trim_start_matches("../").split('/').next().unwrap();
                assert!(
                    dockerfile.contains(&format!("COPY {top}")),
                    "{file} embeds {path}, but the Dockerfile never copies {top} — \
                     the release build will fail while cargo test passes"
                );
            }
        }
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
    async fn every_declared_prompt_can_actually_be_fetched() {
        let api = crate::server::tests::api_with_data();
        let list = rpc_call(api.clone(), "prompts/list", json!({})).await;
        let names: Vec<String> = list["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"why_did_deploy_fail".to_string()), "{names:?}");
        assert!(names.contains(&"explore_data".to_string()), "{names:?}");

        // a prompt in the catalog that prompts/get does not answer is a dead
        // entry the agent only discovers when it asks for it
        for name in &names {
            let got = rpc_call(
                api.clone(),
                "prompts/get",
                json!({ "name": name, "arguments": { "slug": "codo" } }),
            )
            .await;
            let body = got["result"]["messages"][0]["content"]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} is listed but not served: {got}"));
            assert!(body.contains("codo"), "{name} ignores its slug: {body}");
        }

        let deploy = rpc_call(
            api.clone(),
            "prompts/get",
            json!({ "name": "why_did_deploy_fail", "arguments": { "slug": "codo" } }),
        )
        .await;
        let body = deploy["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        let at = |needle: &str| body.find(needle).unwrap_or(usize::MAX);
        assert!(at("project_status") < at("tail_logs"), "status before logs: {body}");
        assert!(body.contains(".env"), "it knows where the .env goes wrong: {body}");

        // the data prompt must not send the agent writing
        let data = rpc_call(
            api.clone(),
            "prompts/get",
            json!({ "name": "explore_data", "arguments": { "slug": "codo", "question": "how many users" } }),
        )
        .await;
        let body = data["result"]["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(body.contains("how many users"), "the question is carried through: {body}");
        assert!(at2(body, "db_info") < at2(body, "db_query"), "schema before SQL: {body}");
        assert!(body.contains("Stay read-only"), "{body}");

        let unknown = rpc_call(api, "prompts/get", json!({ "name": "nope" })).await;
        assert!(unknown["error"]["message"].as_str().unwrap().contains("unknown prompt"));
    }
    fn at2(body: &str, needle: &str) -> usize {
        body.find(needle).unwrap_or(usize::MAX)
    }

    #[tokio::test]
    async fn creating_a_project_through_mcp_goes_through_the_same_wizard() {
        let _env = crate::testutil::env_lock();
        let base = crate::server::tests::mock_github().await;
        std::env::set_var("WEBO_GITHUB_API_BASE", &base);
        std::env::set_var("WEBO_GITHUB_TOKEN", "test-token");
        std::env::set_var("WEBO_DEPLOY_TOKEN", "deploy-secret");
        let api = crate::server::tests::api_with_data();

        let body = tool_text(
            api.clone(),
            "create_project",
            json!({ "repo_owner": "muri", "repo_name": "axofin" }),
        )
        .await;
        assert!(body.contains("rails"), "the template is named: {body}");
        assert!(body.contains(".github/workflows/deploy.yml"), "the files are listed: {body}");
        assert!(
            body.to_lowercase().contains("nothing has been deployed")
                || body.to_lowercase().contains("not deployed"),
            "it must not read as a finished deploy: {body}"
        );
        // the project really exists now, exactly as the panel would have made it
        let p = api.store.project_by_slug("axofin").unwrap().unwrap();
        assert_eq!(p.source, "registered");

        // a stack with no template is refused in words, not with an error
        let body = tool_text(
            api.clone(),
            "create_project",
            json!({ "repo_owner": "muri", "repo_name": "notas" }),
        )
        .await;
        assert!(body.contains("Python"), "{body}");
        assert!(body.contains("no template"), "{body}");

        let out = rpc_call(api, "tools/call", json!({ "name": "create_project", "arguments": {} })).await;
        assert!(out["error"]["message"].as_str().unwrap().contains("repo_owner"));

        std::env::remove_var("WEBO_GITHUB_API_BASE");
        std::env::remove_var("WEBO_GITHUB_TOKEN");
        std::env::remove_var("WEBO_DEPLOY_TOKEN");
    }

    #[tokio::test]
    async fn environment_variables_are_set_and_removed_but_never_shown() {
        let api = crate::server::tests::api_with_data();
        let p = api.store.project_by_slug("codo").unwrap().unwrap();
        api.store.set_env(p.id, "STRIPE_KEY", "sk_live_51H8totallysecret", false).unwrap();

        let body = tool_text(api.clone(), "project_env", json!({ "slug": "codo" })).await;
        assert!(body.contains("STRIPE_KEY"), "the key is listed: {body}");
        assert!(!body.contains("51H8totallysecret"), "the value leaked: {body}");

        // setting one answers with the key, never with what was written
        let body = tool_text(
            api.clone(),
            "project_env",
            json!({ "slug": "codo", "action": "set", "key": "SMTP_PASSWORD", "value": "hunter2hunter2" }),
        )
        .await;
        assert!(body.contains("SMTP_PASSWORD"), "{body}");
        assert!(!body.contains("hunter2hunter2"), "the value leaked: {body}");
        assert_eq!(
            api.store.env_vars(p.id).unwrap().iter().find(|v| v.key == "SMTP_PASSWORD").unwrap().value,
            "hunter2hunter2",
            "it was really stored"
        );

        // a name the shell could not export is refused before it is stored
        let body = tool_text(
            api.clone(),
            "project_env",
            json!({ "slug": "codo", "action": "set", "key": "not a key", "value": "x" }),
        )
        .await;
        assert!(body.contains("not a valid variable name"), "{body}");

        // a managed variable belongs to webo
        api.store.set_env(p.id, "DATABASE_URL", "postgres://x", true).unwrap();
        let body = tool_text(
            api.clone(),
            "project_env",
            json!({ "slug": "codo", "action": "set", "key": "DATABASE_URL", "value": "postgres://mine" }),
        )
        .await;
        assert!(body.contains("managed by webo"), "{body}");
        let body = tool_text(
            api.clone(),
            "project_env",
            json!({ "slug": "codo", "action": "delete", "key": "DATABASE_URL" }),
        )
        .await;
        assert!(body.contains("managed by webo") || body.contains("does not exist"), "{body}");

        // set with no value says what is missing instead of storing an empty one
        let body = tool_text(
            api.clone(),
            "project_env",
            json!({ "slug": "codo", "action": "set", "key": "ONLY_A_KEY" }),
        )
        .await;
        assert!(body.contains("needs key and value"), "{body}");

        let body = tool_text(
            api.clone(),
            "project_env",
            json!({ "slug": "codo", "action": "delete", "key": "SMTP_PASSWORD" }),
        )
        .await;
        assert!(body.contains("Removed SMTP_PASSWORD"), "{body}");
        assert!(
            !api.store.env_vars(p.id).unwrap().iter().any(|v| v.key == "SMTP_PASSWORD"),
            "it is really gone"
        );
        let body = tool_text(api.clone(), "project_env", json!({ "slug": "codo", "action": "delete" })).await;
        assert!(body.contains("needs a key"), "{body}");
    }

    #[tokio::test]
    async fn a_domain_is_connected_and_disconnected_through_mcp() {
        let _env = crate::testutil::env_lock();
        use axum::routing::{delete as axdelete, get as axget, post as axpost};
        let router = axum::Router::new()
            .route(
                "/zones/{z}/dns_records",
                axpost(|| async { axum::Json(json!({"success": true, "result": {"id": "rec1"}})) })
                    .get(|| async { axum::Json(json!({"success": true, "result": [{"id": "rec1"}]})) }),
            )
            .route(
                "/zones/{z}/dns_records/{id}",
                axdelete(|| async { axum::Json(json!({"success": true, "result": {}})) }),
            )
            .route(
                "/accounts/{a}/cfd_tunnel/{t}/configurations",
                axget(|| async {
                    axum::Json(json!({"success": true, "result": {"config": {"ingress": [
                        {"service": "http_status:404"}]}}}))
                })
                .put(|| async { axum::Json(json!({"success": true, "result": {}})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        std::env::set_var("WEBO_CF_API_BASE", format!("http://{addr}"));
        std::env::set_var("CLOUDFLARE_API_TOKEN", "t");
        std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "acc");
        std::env::set_var("CLOUDFLARE_ZONE_ID", "zone");
        std::env::set_var("WEBO_TUNNEL_ID", "tun");
        std::env::set_var("WEBO_APPS_ZONE", "example.com");

        let api = crate::server::tests::api_with_data();
        let body = tool_text(
            api.clone(),
            "connect_domain",
            json!({ "slug": "codo", "domain": "https://loja.example.com/" }),
        )
        .await;
        assert!(body.contains("loja.example.com"), "{body}");
        assert_eq!(
            api.store.project_by_slug("codo").unwrap().unwrap().custom_domain.as_deref(),
            Some("loja.example.com"),
            "the domain is really stored: {body}"
        );

        // a domain outside our zone: webo routes it and says what the owner must do
        let body = tool_text(
            api.clone(),
            "connect_domain",
            json!({ "slug": "codo", "domain": "app.terceiros.com" }),
        )
        .await;
        assert!(body.contains("cfargotunnel.com"), "it hands over the CNAME target: {body}");

        let body = tool_text(api.clone(), "connect_domain", json!({ "slug": "codo", "domain": "" })).await;
        assert!(!body.is_empty(), "an empty domain answers something: {body}");

        // disconnecting is the same tool with no domain
        let body = tool_text(api.clone(), "connect_domain", json!({ "slug": "codo", "action": "disconnect" })).await;
        assert!(body.to_lowercase().contains("disconnect") || body.to_lowercase().contains("removed"), "{body}");
        assert_eq!(
            api.store.project_by_slug("codo").unwrap().unwrap().custom_domain,
            None,
            "it is really gone"
        );

        for var in ["WEBO_CF_API_BASE", "CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_ZONE_ID", "WEBO_TUNNEL_ID", "WEBO_APPS_ZONE"] {
            std::env::remove_var(var);
        }
    }

    #[tokio::test]
    async fn phase_two_tools_declare_that_they_write() {
        let out = rpc_call(crate::server::tests::api_with_data(), "tools/list", json!({})).await;
        let tools = out["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 19, "eleven readers plus eight writers");
        let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();

        // the writers are honest, and the two that can lose data say so
        for n in ["db_query", "db_backup", "triage_errors"] {
            assert_eq!(by_name(n)["annotations"]["readOnlyHint"], false, "{n}");
        }
        assert_eq!(by_name("db_backup")["annotations"]["destructiveHint"], true, "restore overwrites");
        assert_eq!(by_name("triage_errors")["annotations"]["destructiveHint"], true, "delete drops history");
        assert_eq!(
            by_name("db_query")["annotations"]["destructiveHint"], false,
            "db_query reads by default; the backup is what makes a write reversible"
        );
        // and the descriptions name the guard, so the agent knows how to proceed
        assert!(by_name("db_query")["description"].as_str().unwrap().contains("write:true"));
        assert!(by_name("db_backup")["description"].as_str().unwrap().contains("confirm:true"));
        assert!(by_name("triage_errors")["description"].as_str().unwrap().contains("confirm:true"));
    }

    #[tokio::test]
    async fn a_write_without_the_flag_is_refused_and_explains_itself() {
        let api = crate::server::tests::api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        api.store.set_database(id, &crate::store::Database {
            kind: "postgres".into(), container: Some("codo-db".into()), db_name: Some("codo".into()),
            username: Some("codo".into()), password: Some("x".into()), volume: None,
            file_path: None, persisted: true, created_at: 1,
        }).unwrap();

        let text = tool_text(api, "db_query",
            json!({ "slug": "codo", "sql": "DELETE FROM notes WHERE id = 1" })).await;
        assert!(text.contains("changes data"), "{text}");
        assert!(text.contains("write:true"), "it says how to proceed: {text}");
        assert!(text.contains("backup is taken first"), "and what protects it: {text}");
        assert!(!text.contains(" rows"), "nothing ran: {text}");
    }

    #[tokio::test]
    async fn destructive_triage_needs_confirmation_and_says_what_is_lost() {
        let api = crate::server::tests::api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        let i1 = api.store.record_error(id, "f1", "boom", "server", "codo", "boom", 10, None).unwrap();

        // delete without confirm: refused, and it offers the reversible option
        let refused = tool_text(api.clone(), "triage_errors",
            json!({ "slug": "codo", "issue_ids": [i1], "action": "delete" })).await;
        assert!(refused.contains("confirm:true"), "{refused}");
        assert!(refused.contains("Resolving instead"), "it names the safer option: {refused}");
        assert_eq!(api.store.issues(id, None).unwrap().len(), 1, "nothing was deleted");

        // resolve needs no confirmation and reports the new counts
        let resolved = tool_text(api.clone(), "triage_errors",
            json!({ "slug": "codo", "issue_ids": [i1], "action": "resolve" })).await;
        assert!(resolved.contains("1 issue(s)"), "{resolved}");
        assert!(resolved.contains("0 open, 1 resolved"), "{resolved}");
        assert!(resolved.contains("reopens by itself"), "it explains what resolve means: {resolved}");

        // ignoring warns that it silences future occurrences
        let ignored = tool_text(api.clone(), "triage_errors",
            json!({ "slug": "codo", "issue_ids": [i1], "action": "ignore" })).await;
        assert!(ignored.contains("stay ignored even when the error happens again"), "{ignored}");
        assert_eq!(api.store.issue_counts(id).unwrap(), (0, 0, 1));

        // an unknown id is caught before anything runs
        let unknown = tool_text(api.clone(), "triage_errors",
            json!({ "slug": "codo", "issue_ids": [9999], "action": "resolve" })).await;
        assert!(unknown.contains("no issue(s) #9999"), "{unknown}");
        assert!(unknown.contains("Existing ids"), "{unknown}");

        // and a bad action does not silently do nothing
        let bad = tool_text(api, "triage_errors",
            json!({ "slug": "codo", "issue_ids": [i1], "action": "explode" })).await;
        assert!(bad.contains("is not an action"), "{bad}");
    }

    #[tokio::test]
    async fn database_tools_are_clear_about_what_they_cannot_do() {
        let api = crate::server::tests::api_with_data();
        let info = tool_text(api.clone(), "db_info", json!({ "slug": "codo" })).await;
        assert!(info.contains("has no database"), "{info}");
        let backup = tool_text(api.clone(), "db_backup", json!({ "slug": "codo" })).await;
        assert!(backup.contains("has no database"), "{backup}");

        // sqlite is honest about not being backed up
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        api.store.set_database(id, &crate::store::Database {
            kind: "sqlite".into(), container: None, db_name: None, username: None, password: None,
            volume: Some("codo-data".into()), file_path: Some("/data/app.db".into()),
            persisted: false, created_at: 1,
        }).unwrap();
        let sqlite_backup = tool_text(api.clone(), "db_backup", json!({ "slug": "codo" })).await;
        assert!(sqlite_backup.contains("only backs up Postgres"), "{sqlite_backup}");

        // db_rows needs a table, and never interpolates one it did not verify
        let no_table = tool_text(api, "db_rows", json!({ "slug": "codo" })).await;
        assert!(no_table.contains("needs a table"), "{no_table}");
    }

    /// The database tools against a real Postgres — the only way to exercise
    /// the paths that matter (a write taking a backup first, a restore putting
    /// the data back). Skips where no docker daemon is reachable.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_database_tools_work_against_a_real_postgres() {
        let docker = std::process::Command::new("docker")
            .args(["info"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !docker {
            eprintln!("docker unavailable — skipping the live database tools test");
            return;
        }
        let _env = crate::testutil::env_lock();
        let slug = format!("webomcp{}", std::process::id());
        let net = format!("{slug}-net");
        let _ = std::process::Command::new("docker").args(["network", "create", &net]).output();
        std::env::set_var("WEBO_APP_NETWORK", &net);
        let dir = std::env::temp_dir().join(format!("webo-mcpbk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("WEBO_BACKUPS_DIR", dir.to_string_lossy().to_string());

        let api = crate::server::tests::api_with_data();
        api.store.upsert_discovered(&slug, &slug, None, None, 1).unwrap();
        let id = api.store.project_by_slug(&slug).unwrap().unwrap().id;
        let db = crate::db::create_postgres(&slug, &net, "17").await.expect("postgres");
        api.store.set_database(id, &db).unwrap();
        crate::db::pg_query(&db, &net, "CREATE TABLE notes (id int, body text); INSERT INTO notes VALUES (1, 'ola');", true)
            .await
            .expect("seed");

        // db_info names the engine and lists the table
        let info = tool_text(api.clone(), "db_info", json!({ "slug": slug })).await;
        assert!(info.contains("postgres in its own container"), "{info}");
        assert!(info.contains("notes"), "{info}");

        // db_rows reads it back, aligned, with the total
        let rows = tool_text(api.clone(), "db_rows", json!({ "slug": slug, "table": "notes" })).await;
        assert!(rows.contains("ola"), "{rows}");
        assert!(rows.contains("of 1"), "the total is stated: {rows}");
        // an unknown table is refused by name, never interpolated
        let bad_table = tool_text(api.clone(), "db_rows", json!({ "slug": slug, "table": "nope" })).await;
        assert!(bad_table.contains("has no table 'nope'"), "{bad_table}");
        // and an invalid order column too
        let bad_order = tool_text(
            api.clone(), "db_rows",
            json!({ "slug": slug, "table": "notes", "order_by": "id; DROP TABLE notes" }),
        ).await;
        assert!(bad_order.contains("not a valid column name"), "{bad_order}");

        // a read through db_query needs no flag
        let read = tool_text(api.clone(), "db_query", json!({ "slug": slug, "sql": "SELECT body FROM notes" })).await;
        assert!(read.contains("ola"), "{read}");

        // a write takes a backup FIRST and says which file
        let write = tool_text(
            api.clone(), "db_query",
            json!({ "slug": slug, "sql": "INSERT INTO notes VALUES (2, 'segunda')", "write": true }),
        ).await;
        assert!(write.starts_with("Backed up to "), "the dump comes before the write: {write}");
        assert!(write.contains(".sql.gz"), "{write}");
        let after = tool_text(api.clone(), "db_query", json!({ "slug": slug, "sql": "SELECT count(*) FROM notes" })).await;
        assert!(after.contains('2'), "the write landed: {after}");

        // taking one on demand reaches pg_dump and comes back named
        let created = tool_text(api.clone(), "db_backup", json!({ "slug": slug, "action": "create" })).await;
        assert!(created.contains("Backed up"), "{created}");

        // Listing reads the mounted volume, which in production is the same
        // place the helper wrote to; here the helper writes into the docker
        // volume and the test reads a temp dir, so the file is planted. The
        // real dump→restore roundtrip is covered in backups.rs.
        let empty = tool_text(api.clone(), "db_backup", json!({ "slug": slug })).await;
        assert!(empty.contains("no backups yet"), "an empty list says so: {empty}");
        std::fs::create_dir_all(dir.join(&slug)).unwrap();
        std::fs::write(dir.join(&slug).join("20260907-040000.sql.gz"), b"planted").unwrap();
        let listed = tool_text(api.clone(), "db_backup", json!({ "slug": slug })).await;
        assert!(listed.contains("20260907-040000.sql.gz"), "{listed}");
        assert!(listed.contains("7 B"), "the size is formatted: {listed}");

        // restore refuses without confirmation, and says what it would do
        let refused = tool_text(
            api.clone(), "db_backup",
            json!({ "slug": slug, "action": "restore", "file": "20260907-040000.sql.gz" }),
        ).await;
        assert!(refused.contains("confirm:true"), "{refused}");
        assert!(refused.contains("overwrite"), "it says what happens: {refused}");
        // and restore without a file name says which argument is missing
        let no_file = tool_text(api.clone(), "db_backup", json!({ "slug": slug, "action": "restore" })).await;
        assert!(no_file.contains("needs the file name"), "{no_file}");

        crate::db::drop_postgres(db.container.as_deref().unwrap(), db.volume.as_deref()).await.ok();
        let _ = std::process::Command::new("docker").args(["network", "rm", &net]).output();
        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var("WEBO_BACKUPS_DIR");
        std::env::remove_var("WEBO_APP_NETWORK");
    }

    #[tokio::test]
    async fn every_write_tool_is_annotated_and_audited() {
        let out = rpc_call(crate::server::tests::api_with_data(), "tools/list", json!({})).await;
        let tools = out["result"]["tools"].as_array().unwrap();
        // the audit list and the annotations must agree, or a write happens
        // with no record of it
        for t in tools {
            let name = t["name"].as_str().unwrap();
            let read_only = t["annotations"]["readOnlyHint"] == true;
            assert_eq!(
                !read_only,
                WRITE_TOOLS.contains(&name),
                "{name}: annotation and the audit list disagree"
            );
        }
        let by_name = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();
        // only deleting a project is marked destructive among the new ones:
        // the others can be undone or simply redone
        assert_eq!(by_name("delete_project")["annotations"]["destructiveHint"], true);
        for n in ["create_project", "deploy_project", "project_env", "connect_domain"] {
            assert_eq!(by_name(n)["annotations"]["destructiveHint"], false, "{n}");
        }
        // create_project promises not to deploy, and delete says what it spares
        assert!(by_name("create_project")["description"].as_str().unwrap().contains("does NOT deploy"));
        assert!(by_name("delete_project")["description"].as_str().unwrap().contains("NEVER removed"));
        assert!(by_name("project_env")["description"].as_str().unwrap().contains("ALWAYS masked"));
    }

    #[tokio::test]
    async fn deleting_a_project_needs_the_slug_and_spares_the_data() {
        let api = crate::server::tests::api_with_data();
        // the wrong confirmation refuses and explains the rule
        let refused = tool_text(
            api.clone(), "delete_project",
            json!({ "slug": "codo", "confirm": "yes" }),
        ).await;
        assert!(refused.contains("confirm exactly equal to the slug"), "{refused}");
        assert!(refused.contains("NEVER removed through MCP"), "it says what it will not touch: {refused}");
        assert!(api.store.project_by_slug("codo").unwrap().is_some(), "nothing was deleted");

        // and webo refuses to delete itself whatever the confirmation
        api.store.upsert_discovered("webo", "webo", None, None, 1).unwrap();
        let itself = tool_text(api, "delete_project", json!({ "slug": "webo", "confirm": "webo" })).await;
        assert!(itself.contains("cannot delete itself"), "{itself}");
    }

    #[tokio::test]
    async fn env_values_never_come_back_in_clear_text() {
        let api = crate::server::tests::api_with_data();
        let id = api.store.project_by_slug("codo").unwrap().unwrap().id;
        api.store.set_env(id, "STRIPE_SECRET_KEY", "sk_live_verysecret_do_not_leak", false).unwrap();
        api.store.set_env(id, "DATABASE_URL", "postgres://u:p@h/db", true).unwrap();

        let listed = tool_text(api.clone(), "project_env", json!({ "slug": "codo" })).await;
        assert!(listed.contains("STRIPE_SECRET_KEY"), "the name is shown: {listed}");
        assert!(
            !listed.contains("verysecret"),
            "the value must never reach the model's context: {listed}"
        );
        assert!(listed.contains("managed by webo"), "a managed variable is marked: {listed}");
        assert!(listed.contains("never returns"), "it states the rule: {listed}");
        // the internal ingest key is not a variable of the app
        assert!(!listed.contains("__WEBO_"), "{listed}");

        // a managed variable cannot be overwritten by hand
        let managed = tool_text(
            api.clone(), "project_env",
            json!({ "slug": "codo", "action": "set", "key": "DATABASE_URL", "value": "x" }),
        ).await;
        assert!(managed.contains("managed by webo"), "{managed}");

        // and an invalid name is refused before touching anything
        let bad = tool_text(
            api, "project_env",
            json!({ "slug": "codo", "action": "set", "key": "bad key!", "value": "x" }),
        ).await;
        assert!(bad.contains("not a valid variable name"), "{bad}");
    }

    #[tokio::test]
    async fn create_project_is_clear_that_it_did_not_deploy() {
        let _env = crate::testutil::env_lock();
        std::env::remove_var("WEBO_GITHUB_TOKEN");
        let api = crate::server::tests::api_with_data();
        // with no token the answer says what is missing, it does not panic
        let out = rpc_call(
            api,
            "tools/call",
            json!({ "name": "create_project", "arguments": { "repo_owner": "muri", "repo_name": "x" } }),
        )
        .await;
        assert!(
            out["error"]["message"].as_str().unwrap().contains("github token"),
            "{out}"
        );
        // and missing arguments are named
        let api2 = crate::server::tests::api_with_data();
        let no_args = rpc_call(api2, "tools/call", json!({ "name": "create_project" })).await;
        assert!(no_args["error"]["message"].as_str().unwrap().contains("repo_owner"));
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
