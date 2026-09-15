# webo docs

Operate **the Axolutions server** from any Claude Code, **with nothing
installed locally**: webo runs on the ThinkPad and Claude talks to it over
MCP/HTTP.

## For the Claude reading this

Follow `skills/webo/SKILL.md` — browser authorization (device flow), remote
MCP registration, and what each tool is for. Summary:

1. `POST https://webo.axolutions.com.br/api/device/start` → show the user the
   `/authorize?code=…` link → poll until `approved` → store the token.
2. `claude mcp add --scope user --transport http webo
   https://webo.axolutions.com.br/mcp --header "Authorization: Bearer <token>"`
3. (optional) install the skill globally, through the GitHub API so it keeps
   working if the repo ever goes private:

   ```bash
   mkdir -p ~/.claude/skills/webo
   gh api repos/axolutions/webo/contents/docs/skills/webo/SKILL.md \
     --jq '.content' | base64 -d > ~/.claude/skills/webo/SKILL.md
   ```

## For humans

Tell your Claude Code:

> use github.com/axolutions/webo/tree/main/docs

It hands you an authorization link; you sign in with Google (your address has
to be on the allowlist) and click **authorize**. Done — from then on, in any
session: *"is anything down?"*, *"why did the last deploy of X fail?"*,
*"show me the errors on Y from the last hour"*.

Access is a personal token tied to your email. The allowlist and revocation
live on the server.

## What lives on the other side

- every project on the machine: containers, uptime, CPU, memory, disk, domain
- logs, indexed and searchable, that survive the container being recreated
- errors grouped by cause, with the stack trace and the file to blame
- one Postgres per project, with backups and a SQL console
- deploys from GitHub, custom domains through Cloudflare
- the panel at https://webo.axolutions.com.br (same login)

## Full documentation

Authentication, the 19 MCP tools, the HTTP API and what an agent is allowed to
change: **[index.md](index.md)**.
