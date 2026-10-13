---
name: {NAME}
description: The server at {HOST}, watched by webo — projects, deploys, logs, errors, databases and machine health. Use when the user asks whether something on {HOST} is up or down, why a deploy failed, what an app is logging or erroring, how much CPU/memory/disk anything is using, to query a project's database, to deploy a repository, or to set up access to this server. Nothing runs locally — everything is an HTTP call to the server.
---

# {NAME} — the server at {HOST}, from here

webo watches one server: the projects running on it, their deploys, logs,
errors and databases, and the machine itself. You talk to it over **MCP over
HTTP**. Nothing is installed on this machine.

- Server, panel and MCP: `{BASE}`

## 1. Already configured?

If `mcp__{NAME}__*` tools (server_health, list_projects…) are in the session,
skip to "How to use". Otherwise check `claude mcp list`; if `{NAME}` is not
there, run the setup.

## 2. Setup (once per machine)

Fetch `{BASE}/mcp` and follow it. It walks through the browser authorization,
registers the MCP and installs this skill.

A 401 after it worked means the token was revoked or the email came off the
server's allowlist: run the setup again.

## 3. How to use

Read before you conclude. `webo://runbook` (an MCP resource) is how the
machine is actually put together — conventions that, when broken, produce
exactly the symptoms you are looking at.

| intent | tool |
|---|---|
| is the machine healthy? | `server_health` (CPU, memory, disk, temperature, battery, network, uptime) |
| what is eating the CPU? | `server_processes` |
| what is on this server? | `list_projects` — up/down, tech, domain, open errors, last deploy |
| one project in full | `project_status` — containers, uptime, restarts, disk, domain, last build |
| resource use over time | `project_metrics` — already summarised: average, peak with its timestamp, trend |
| find something in the logs | `search_logs` (indexed, survives redeploys) · `tail_logs` (live from the container) |
| what is broken | `list_errors` (grouped by cause) → `error_detail` (stack trace, blamed file) |
| the database | `db_info` → `db_rows` (browse) → `db_query` (SQL) |
| backups | `db_backup` — list, take one, restore |
| clean up issues | `triage_errors` — resolve, ignore, reopen, delete |
| deploy a repo | `create_project` (registers, detects the stack) → `deploy_project` (actually builds) |
| variables | `project_env` — list, set, remove |
| custom domain | `connect_domain` |
| take a project down | `delete_project` — containers only |

Three prompts carry the order of operations that avoids dead ends:
`diagnose_project`, `why_did_deploy_fail`, `explore_data`.

## 4. What you may change, and what you may not

The limits are enforced on the server, not by convention — but know them, so
you do not promise the user something that will be refused:

- **Every write is logged.** Each writing call prints one line into webo's own
  logs (`[webo-mcp] tool args -> outcome`), visible in the panel. Refusals too.
- **A database write takes a backup first.** `db_query` with `write:true` dumps
  the database before running and names the file. If the backup fails, the
  statement does not run.
- **`delete_project` removes containers only.** It needs the slug repeated in
  `confirm`. Volumes (the data) and images are never touched through MCP;
  dropping data is a panel action. webo refuses to delete itself.
- **Environment values never come back in clear text** — keys are listed,
  values masked, in both directions.
- **`create_project` deploys nothing.** It registers and plans; `deploy_project`
  is what commits and starts the build. Do not report a deploy that has not
  happened.

Conventions worth keeping:

- Answer from evidence. If the tools do not support a conclusion, say what is
  missing instead of guessing.
- Prefer the summarised answers (`project_metrics`, `list_errors`) over dumping
  raw series or log lines at the user.
- Deploys run on GitHub Actions and take minutes: after `deploy_project`, check
  back with `project_status` rather than claiming it is up.

## 5. HTTP API (fallback without MCP — same auth, same rules)

```bash
BASE={BASE}
A='Authorization: Bearer webo_…'
curl -s $BASE/api/v1/projects -H "$A"                          # every project
curl -s $BASE/api/v1/projects/<slug> -H "$A"                   # one, in full
curl -s "$BASE/api/v1/projects/<slug>/logs?q=error" -H "$A"    # logs
curl -s $BASE/api/v1/projects/<slug>/errors -H "$A"            # grouped errors
curl -s $BASE/api/v1/snapshot -H "$A"                          # the machine now
curl -s $BASE/mcp -H "$A" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

## 6. Update this skill

```bash
curl -fsS {BASE}/skill.md -o ~/.claude/skills/{NAME}/SKILL.md
```

One markdown file — nothing else is installed.
