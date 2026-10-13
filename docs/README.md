# webo docs

Operate a webo server from any AI agent, **with nothing installed locally**:
webo runs on the server and the agent talks to it over MCP/HTTP.

## For humans

Paste this into Claude Code (or another agent), with your server's address:

> Install the webo MCP and skill from https://<your-webo>/mcp

The agent hands you an authorization link; you sign in (your address has to be
on the server's allowlist) and click **authorize**. Done — from then on, in any
session: *"is anything down?"*, *"why did the last deploy of X fail?"*,
*"show me the errors on Y from the last hour"*.

Access is a personal token tied to your email. The allowlist and revocation
live on the server.

## For the agent reading this

`GET https://<your-webo>/mcp` answers with the install guide for that server:
browser authorization (device flow), MCP registration and the skill, already
filled in with the server's address and name. Follow it. The same server serves
the skill at `/skill.md`.

## What lives on the other side

- every project on the machine: containers, uptime, CPU, memory, disk, domain
- logs, indexed and searchable, that survive the container being recreated
- errors grouped by cause, with the stack trace and the file to blame
- one Postgres per project, with backups and a SQL console
- deploys from GitHub, custom domains through Cloudflare
- the panel at the server's own address (same login)

## Full documentation

Authentication, the 19 MCP tools, the HTTP API and what an agent is allowed to
change: **[index.md](index.md)**.
