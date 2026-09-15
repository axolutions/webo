# HTTP API

Everything the panel shows comes from these paths, and the MCP server reads the
same code. Nothing is UI-only.

All of it needs `Authorization: Bearer <credential>` when login is on — see
[authentication.md](authentication.md).

## The machine

| path | returns |
|---|---|
| `GET /api/v1/snapshot` | everything about the current moment |
| `GET /api/v1/history?minutes=1440` | CPU/memory/network series |
| `GET /api/v1/processes` | the process table |
| `GET /api/v1/system` | hostname, OS, kernel, hardware |
| `GET /api/v1/docker` | images, volumes, what a prune would free |
| `GET /healthz` | `ok` — public |

## Projects

| path | returns |
|---|---|
| `GET /api/v1/projects` | every project with its live numbers |
| `POST /api/v1/projects` | register a GitHub repo (`{repo_owner, repo_name}`) |
| `GET /api/v1/projects/{slug}` | one project in full |
| `POST /api/v1/projects/{slug}/provision` | deploy it |
| `DELETE /api/v1/projects/{slug}` | tear it down (what is removed is in the body) |
| `PUT · DELETE /api/v1/projects/{slug}/domain` | connect or disconnect a domain |
| `GET · PUT · DELETE /api/v1/projects/{slug}/env` | variables (values masked on read) |
| `GET /api/v1/projects/{slug}/logs` | indexed logs, with `q`, `level`, `container` |
| `GET /api/v1/projects/{slug}/errors` | issues grouped by cause |
| `GET /api/v1/projects/{slug}/history` | that project's series |

## Database

| path | returns |
|---|---|
| `GET · POST · DELETE /api/v1/projects/{slug}/database` | the project's database |
| `GET /api/v1/projects/{slug}/database/tables` | tables and row counts |
| `GET /api/v1/projects/{slug}/database/tables/{table}/rows` | browse |
| `POST /api/v1/projects/{slug}/database/query` | SQL |
| `GET · POST /api/v1/projects/{slug}/database/backups` | list, take one |

## Login

| path | returns |
|---|---|
| `GET /api/config` | which mode the server is in — public |
| `POST /api/device/start` | opens a device code — public |
| `GET /api/device/poll?code=` | the token, once approved — public |
| `POST /api/device/approve` | approves a code (needs a **session**) |
| `GET /api/team` | the people in the Clerk instance |
| `GET /api/tokens` · `DELETE /api/tokens/{hash}` | list and revoke personal tokens |

## Errors from the browser

`POST /api/v1/ingest/{key}` is public by design: it is the snippet running in a
visitor's browser on a deployed app, authenticated by the project's ingest key.
It sends `text/plain` on purpose — `application/json` would need a CORS
preflight, and `sendBeacon` cannot preflight.
