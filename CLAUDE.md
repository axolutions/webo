# webo

Self-hosted server health panel. One Rust binary (axum + manual /proc
scanning), vanilla front embedded via `include_str!`, versioned JSON API
(`/api/v1/*`) that is the single contract for the UI and for future MCP/
automation consumers. Open-source project (MIT).

## I18N — ALWAYS

- **Every user-facing string goes through i18n. No hardcoded UI copy.**
- **Default language: English.** The UI switches based on the browser
  language (`navigator.language`).
- **Two languages: English (`en`, default) and Portuguese (`pt`).**
  Adding a string means adding it to both dictionaries.
- **Code and documentation are English-only** (identifiers, comments,
  commit messages, README, docs). Only the i18n dictionaries carry
  Portuguese.
- Number/date formatting follows the active language's locale.

## Conventions

- The panel must keep working (gracefully hiding cards) when a metric
  source is unavailable — never assume specific hardware or mounts.
- Nothing UI-only: any data the panel shows must come from `/api/v1/*`.
- Detected process technology (`kind`) is only what the binary name
  honestly reveals — never guess an app's purpose.
- Deploys: push to `main` → GitHub Actions (axolutions/webo-deploy) →
  GHCR → the server pulls. `deploy/docker-compose.yml` is the generic
  OSS compose; `deploy/docker-compose.homelab.yml` is our overlay.
