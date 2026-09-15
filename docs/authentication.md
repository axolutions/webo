# Authentication

## The two modes

One binary; the **environment** picks the mode:

| | local | server |
|---|---|---|
| when | no Clerk keys | `CLERK_PUBLISHABLE_KEY` + `CLERK_SECRET_KEY` |
| auth | none — everything open (one person, one machine) | **every** path needs `Authorization: Bearer …` |
| public exceptions | — | the shell (`/`, `/authorize`), `/api/config`, `/api/device/start`, `/api/device/poll`, `/healthz`, and the browser error ingest |
| who gets in | — | Google login through Clerk, narrowed by an allowlist |

One key without the other is refused at startup. A half-configured login that
quietly serves the panel open is the failure this exists to prevent.

`GET /api/config` (always public) says which mode the server is in:

```json
{ "mode": "server", "clerk_publishable_key": "pk_…" }   // or { "mode": "local" }
```

## Two kinds of credential

1. **Clerk session JWT** — what the signed-in panel sends. Short-lived,
   refreshed by the browser SDK. Not usable for automation.
2. **Personal `webo_…` token** — long-lived, issued by the flow below. This is
   what the MCP client and any script use. The server keeps only a sha256; the
   cleartext appears once, at issuance.

Both travel the same way: `Authorization: Bearer <credential>`.

`WEBO_ALLOWED_EMAILS` (comma-separated) has the last word over both: an address
removed from it stops working on the next request, with no token to hunt down.

## Getting a personal token (device flow)

The "type the code on the TV" flow, in three steps:

```bash
BASE=https://webo.axolutions.com.br

# 1. start (public) — keep the code
curl -s -X POST $BASE/api/device/start
# → { "code": "XXXX-XXXX", "authorize_path": "/authorize?code=XXXX-XXXX",
#     "expires_in": 900, "interval": 3 }

# 2. a person on the team opens it, signs in with Google and clicks authorize:
#    https://webo.axolutions.com.br/authorize?code=XXXX-XXXX
#    (the token carries the approver's email, taken from that session — never
#     from anything the agent sent)

# 3. poll every ~3s until approved (codes live 15 minutes)
curl -s "$BASE/api/device/poll?code=XXXX-XXXX"
# {"status":"pending"}                                → wait
# {"status":"approved","token":"webo_…","email":"…"}  → store it; shown once
# {"status":"delivered"}                              → already handed over; start again
# {"status":"expired"}                                → start again
```

Approving requires a **session**, not a token: one leaked token cannot mint
another.

Test it:

```bash
curl -s $BASE/api/v1/projects -H "Authorization: Bearer webo_…"   # 200 ok · 401 no
```

## Errors, revocation, good practice

- **401** on any path: missing, invalid or revoked credential, or an email off
  the allowlist. The body says which: `{"error":"not authenticated: …"}`.
- Tokens are listed and revoked at `GET /api/tokens` and
  `DELETE /api/tokens/{hash}` — by hash, the only handle that exists after
  issuance.
- One token per machine or automation: revoking one should not take the others
  down.
- A token can operate the server. Treat it like a password: a CI secret, never
  in code.

## The tailnet listener

Port 5051 on the host's Tailscale address serves the same MCP with **no
login**: the tailnet is the credential there. It is the way back in if Clerk is
unreachable, and it is not published to the internet.
