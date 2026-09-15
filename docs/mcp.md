# MCP

One endpoint: `POST https://webo.axolutions.com.br/mcp`, JSON-RPC 2.0,
protocol `2025-06-18`. Register it once:

```bash
claude mcp add --scope user --transport http webo \
  https://webo.axolutions.com.br/mcp --header "Authorization: Bearer webo_…"
```

## Tools (19)

**Reading — the machine**

| tool | answers |
|---|---|
| `server_health` | CPU, memory, disk, temperature, battery, network, uptime, in one answer |
| `server_processes` | the most active process groups, with CPU, memory, disk i/o, threads |

**Reading — projects**

| tool | answers |
|---|---|
| `list_projects` | every project: tech, up or down, domain, CPU/memory, open errors, last deploy |
| `project_status` | one project in full: containers, uptime, restarts, disk, domain, last build |
| `project_metrics` | a window summarised — average, peak with its timestamp, trend — never a raw series |
| `search_logs` | full-text over the indexed logs, by level, resource and window |
| `tail_logs` | the last lines straight from the container, bypassing the index |
| `list_errors` | issues grouped by cause: count, blamed file, first and last seen |
| `error_detail` | one issue's occurrences, with the stack trace |
| `db_info` | what database a project has, and how big |
| `db_rows` | browse a table with paging and ordering, without SQL |

**Writing**

| tool | does | guard |
|---|---|---|
| `db_query` | runs SQL | a write needs `write:true` and takes a backup first; if the backup fails, nothing runs |
| `db_backup` | list, take, restore | restoring overwrites the current data and needs `confirm:true` |
| `triage_errors` | resolve, ignore, reopen, delete issues | deleting drops history and needs confirmation |
| `create_project` | registers a GitHub repo, detects the stack, plans the files | **deploys nothing** |
| `deploy_project` | the first deploy: database, variables, secrets, build | the build runs on GitHub Actions and takes minutes |
| `project_env` | list, set, remove variables | values are masked in both directions, always |
| `connect_domain` | connect or disconnect a custom domain | DNS in our zone is created; outside it, you get the CNAME target |
| `delete_project` | stops and removes the containers | needs the slug in `confirm`; volumes and images are never touched; webo refuses to delete itself |

Every writing call is logged into webo's own logs as
`[webo-mcp] <tool> <arguments> -> <outcome>`, visible in the panel like any
other container's output. Refusals are logged too.

## Prompts (3)

- `diagnose_project` — why a project is failing or slow, in the order that
  avoids dead ends.
- `why_did_deploy_fail` — the failure modes that have actually broken a deploy
  on this server, checked in order.
- `explore_data` — answer a question from a project's database, reading the
  schema before writing SQL.

## Resources (2)

- `webo://runbook` — how the machine is put together and the lessons that cost
  a broken deploy to learn. Read it before proposing a change to the server.
- `webo://projects` — the current inventory, as context rather than a call.
