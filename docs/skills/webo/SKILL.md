---
name: webo
description: The Axolutions server (webo.axolutions.com.br) — projects, deploys, logs, errors, databases and machine health. Use when the user asks whether something is up or down, why a deploy failed, what an app is logging or erroring, how much CPU/memory/disk anything is using, to query a project's database, to deploy a repository, or to set up webo access. Nothing runs locally — everything is an HTTP call to the server.
---

# webo — the server, from here (remote)

webo runs the Axolutions machine: a ThinkPad acting as a small VPS, with every
project on it deployed from GitHub and reachable through a Cloudflare Tunnel.
You talk to it over **MCP over HTTP**. Nothing is installed on this machine.

- Server, panel and MCP: `https://webo.axolutions.com.br`

## 1. Already configured?

If `mcp__webo__*` tools (webo_server_health, webo_list_projects…) are in the
session, skip to "How to use". Otherwise check `claude mcp list`; if webo is
not there, run the setup.

## 2. Setup (once per machine)

### 2.1 Authorization — a person approves in the browser

```bash
curl -s -X POST https://webo.axolutions.com.br/api/device/start
# → { "code": "XXXX-XXXX", "authorize_path": "/authorize?code=XXXX-XXXX",
#     "expires_in": 900, "interval": 3 }
```

Show the user, prominently:

> Open **https://webo.axolutions.com.br/authorize?code=XXXX-XXXX**, sign in
> with Google and click **authorize**. Check that the code on screen is
> **XXXX-XXXX**.

Meanwhile, poll every ~3s (the code is good for 15 minutes):

```bash
curl -s "https://webo.axolutions.com.br/api/device/poll?code=XXXX-XXXX"
# {"status":"pending"}                                → keep waiting
# {"status":"approved","token":"webo_…","email":"…"}  → STORE IT: shown once
# {"status":"expired"} / {"status":"delivered"}       → start over at 2.1
```

Only an address on the server's allowlist can approve. If the user cannot sign
in, their email has to be added on the server.

### 2.2 Test the token

```bash
curl -s https://webo.axolutions.com.br/api/v1/projects -H "Authorization: Bearer webo_…"
# 200 with the project list = working · 401 = redo 2.1
```

### 2.3 Register the MCP (user scope = every future session, any directory)

```bash
claude mcp add --scope user --transport http webo \
  https://webo.axolutions.com.br/mcp \
  --header "Authorization: Bearer webo_…"
```

Confirm with `claude mcp list`. **The tools only load in the next session** —
in this one, use the HTTP API (section 5) with the same Bearer.

A 401 after it worked means the token was revoked or the email came off the
allowlist: redo 2.1.

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
BASE=https://webo.axolutions.com.br
A='Authorization: Bearer webo_…'
curl -s $BASE/api/v1/projects -H "$A"                          # every project
curl -s $BASE/api/v1/projects/<slug> -H "$A"                   # one, in full
curl -s "$BASE/api/v1/projects/<slug>/logs?q=error" -H "$A"    # logs
curl -s $BASE/api/v1/projects/<slug>/errors -H "$A"            # grouped errors
curl -s $BASE/api/v1/snapshot -H "$A"                          # the machine now
curl -s $BASE/mcp -H "$A" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

## 6. Install this skill globally (optional, recommended)

Fetched through the GitHub API, so it keeps working if the repo ever goes
private (`raw.githubusercontent.com` answers 404 on a private repo):

```bash
mkdir -p ~/.claude/skills/webo
gh api repos/axolutions/webo/contents/docs/skills/webo/SKILL.md \
  --jq '.content' | base64 -d > ~/.claude/skills/webo/SKILL.md
```

One markdown file — nothing else is installed.
