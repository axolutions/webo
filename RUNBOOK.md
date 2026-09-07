# Server runbook

How this server is put together, and the lessons that cost a broken deploy to
learn. Read this before proposing any change to the machine — most of what is
here is not guessable from the code.

Served to agents as the MCP resource `webo://runbook`.

## The machine

One ThinkPad (`RD-007661`) running Ubuntu 24.04, acting as a small VPS on the
home network. It has no public IP: everything reaches it through a Cloudflare
Tunnel. The laptop lid stays closed, charging is capped for battery health, so
a battery reading below 100% is normal and not a problem.

Access paths, in order of preference:

1. **The panel** — `webo.axolutions.com.br`, read-only, behind Cloudflare.
2. **The MCP server** — port 5051, bound to the Tailscale IP only.
3. **SSH over Tailscale** — `homelab@<tailscale-ip>`. Last resort.

## Layout on disk

```
/home/homelab/apps/<app>/
  deploy/
    docker-compose.yml            # from the app's repo
    docker-compose.homelab.yml    # GHCR image + homelab network
    .env                          # written by webo, never by hand
```

`~/apps/<app>` belongs to the `homelab` user. webo runs in a container as root
and writes the `.env` through a helper container, so the helper chowns what it
creates back to the owner of `~/apps` — otherwise the deploy, which lands over
SSH as `homelab`, cannot write its compose files. That bug happened; the fix is
in `db::write_app_env`.

## How a deploy works

1. A push to `main` triggers the repo's `.github/workflows/deploy.yml`.
2. The workflow builds the image and pushes it to `ghcr.io/<owner>/<repo>`.
3. It joins the tailnet with an OAuth client tagged `tag:ci`.
4. It streams the compose files to `~/apps/<app>/deploy/` over SSH — **not
   scp**: Tailscale SSH has no scp or sftp subsystem, so each file goes through
   `cat > file` on the shell.
5. It runs `docker compose ... pull && up -d`.

Every app joins the external Docker network `homelab`, which is also where
`cloudflared` lives. That shared network is how the tunnel reaches containers
by name.

## Conventions that matter

- **Compose project name is explicit.** Without a `name:` in the compose file,
  Docker names the project after the *directory* — every app under `deploy/`
  became a project called "deploy" and they collided. Always set `name:`.
- **`env_file` resolves relative to the compose file.** The compose files live
  in `<app>/deploy/`, so the `.env` must be there too. A `.env` one level up is
  silently ignored and the app boots with no configuration.
- **Discovery is by label.** webo finds projects through
  `com.docker.compose.project`. A container without it becomes a
  single-container project named after itself.
- **`webo.role`** on a container marks what it is to the project (`app`,
  `database`). webo's own Postgres containers carry `webo.role=database`.
- **Images are matched by id, not tag.** A container whose `latest` tag moved on
  only references its image by sha256, so image sizes are looked up by both.

## Cloudflare

- One named tunnel, configured remotely through the API — the ingress rules live
  in Cloudflare, not in a local config file.
- Each project gets an obscure automatic hostname on first deploy
  (`three-words.axolutions.com.br`) that **never changes**, plus an optional
  custom domain.
- DNS records are CNAMEs to `<tunnel-id>.cfargotunnel.com`.
- **Route DNS by tunnel UUID, never by name.** Naming works until it silently
  points at the wrong tunnel.
- **Universal SSL covers only one wildcard level** on the free plan.
  `app.axolutions.com.br` gets a certificate; `a.b.axolutions.com.br` does not.
- The 404 catch-all rule must stay last in the ingress list, and webo preserves
  any rule it does not own when it reconciles.

## Databases

- One Postgres container per project, created by webo, named `<slug>-db`, with
  its data in the volume `<slug>-db-data`. The password is generated and never
  serialized out of the store.
- `DATABASE_URL` is a **managed** variable: webo writes it and it cannot be
  edited or deleted by hand.
- Queries from the panel run through short-lived helper containers. Read-only
  mode is enforced with `PGOPTIONS=-c default_transaction_read_only=on` — not
  with a `SET` statement, because psql echoes "SET" as the first output line and
  it becomes the column header.
- SQLite is detected by reading the `SQLite format 3` header from files inside
  the project's volumes. A file in the container layer (not a volume) is
  reported as **not persisted**: every deploy wipes it.
- Backups: daily `pg_dump | gzip` into the shared `webo-backups` volume, seven
  kept. webo mounts that volume at `/backups`, so listing and downloading are
  plain file reads.

## Rails apps, specifically

Four things had to be fixed before a Rails app could deploy at all, and they are
all in the template now:

- The image is built on **the Ruby the app asks for**, read from `.ruby-version`
  or the Gemfile, patch included. bundler compares the full version, so
  `ruby:3.2` fails a Gemfile pinned to `3.2.1`.
- `libpq-dev` (build) and `libpq5` (runtime) are installed, or the `pg` gem has
  nothing to compile against.
- The entrypoint runs `db:prepare` when `DATABASE_URL` is set.
- It generates a `SECRET_KEY_BASE` into the storage volume when none was given,
  because Rails 7.1 refuses to boot in production without one.

Rails also **forces SSL in production**. Behind the tunnel that works because
`cloudflared` sends `X-Forwarded-Proto: https`; a direct `curl` to the container
port gets a 301 and that is expected, not a bug.

## Logs and errors

- Logs are collected from Docker every 10s and indexed in SQLite with FTS5, so
  they survive the container being recreated on deploy. 500 MB per project, and
  the oldest tenth is pruned when the cap is hit.
- Error grouping reads those same lines — no SDK, nothing to install in the app.
- A line that declares its own level (the Ruby Logger's `I, [...] INFO -- :`) is
  believed: an INFO line saying "Completed 500 Internal Server Error" is a
  status, not a new error.
- Rails stamps each line of a request with a request id. It is stripped before
  fingerprinting, or the same bug on two requests becomes two issues.
- Browser errors are optional and arrive through a public ingest endpoint. The
  snippet sends `text/plain` on purpose: `application/json` would need a CORS
  preflight, and `sendBeacon` cannot preflight — it just silently fails.

## When something is wrong

- **App returns 502 through the tunnel** — the container is down or not on the
  `homelab` network. Check `project_status`, then `tail_logs`.
- **Deploy went green but nothing changed** — the image was pushed but compose
  pulled a cached tag, or the `.env` landed in the wrong directory.
- **App boots with no configuration** — check that `.env` is in
  `<app>/deploy/`, not in `<app>/`.
- **A restart with a memory peak just before it** is an OOM kill, not a code
  bug. `project_metrics` shows the peak with its timestamp.
- **Metrics history looks short** — the live series lives in RAM and is lost when
  webo itself is deployed. Windows beyond 24h read the 5-minute aggregates
  persisted in SQLite, which do survive.

## Things never to do

- Do not delete volumes to "clean up". That is the data.
- Do not edit `~/apps/<app>/deploy/.env` by hand — webo rewrites it and your
  change disappears without warning.
- Do not point DNS at the tunnel by name. UUID only.
- Do not run migrations by hand on a project webo deploys; the entrypoint does
  it at boot.
