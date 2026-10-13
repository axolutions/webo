# Install webo for {HOST}

You are an AI agent, and the person you are working with asked you to install
the webo MCP server and skill for {HOST}. Follow these steps in order, run the
commands exactly as written, and talk to the person in their own language.

webo watches the server at {HOST}: its projects, deploys, logs, errors,
databases and machine health. Its MCP server is registered as `{NAME}`.

## 1. Already installed?

```bash
claude mcp list 2>/dev/null | grep -E '^{NAME}:' || echo "not installed"
```

If `{NAME}` shows up connected, skip to step 4.

## 2. Ask for authorization

```bash
curl -s -X POST {BASE}/api/device/start
```

The answer carries a `code` shaped like `XXXX-XXXX`. Show the person, clearly,
with the real code in the link:

> Open **{BASE}/authorize?code=XXXX-XXXX**, sign in and click **authorize**.
> Check that the code on the screen is **XXXX-XXXX**.

Only addresses on the server's allowlist can approve. If the person cannot get
in, an admin of {HOST} has to add their email.

## 3. Wait for the approval and register the MCP

Replace `XXXX-XXXX` with the code and run the script. It waits up to 15
minutes, so run it in the background or with a 15-minute timeout. Once the
person approves, it stores the token in `~/.config/webo/{NAME}.token` and
registers the MCP server by itself. The token never shows on screen: do not ask
for it and do not print it.

```bash
CODE=XXXX-XXXX
for i in $(seq 1 300); do
  R=$(curl -s "{BASE}/api/device/poll?code=$CODE")
  case "$R" in
    *'"approved"'*)
      mkdir -p ~/.config/webo && umask 077
      printf '%s' "$R" | sed -E 's/.*"token":"([^"]+)".*/\1/' > ~/.config/webo/{NAME}.token
      echo "approved by $(printf '%s' "$R" | sed -E 's/.*"email":"([^"]+)".*/\1/')"
      if command -v claude >/dev/null; then
        claude mcp remove {NAME} --scope user >/dev/null 2>&1
        claude mcp add --scope user --transport http {NAME} {BASE}/mcp \
          --header "Authorization: Bearer $(cat ~/.config/webo/{NAME}.token)" >/dev/null \
          && echo "MCP {NAME} registered" || echo "could not register the MCP"
      fi
      break ;;
    *'"expired"'*|*'"delivered"'*) echo "the code expired or was already used: go back to step 2"; break ;;
  esac
  sleep 3
done
```

If the script ends without "approved", go back to step 2.

## 4. Install the skill

```bash
mkdir -p ~/.claude/skills/{NAME} && curl -fsS {BASE}/skill.md -o ~/.claude/skills/{NAME}/SKILL.md && echo "skill installed"
```

## 5. Finish

Check with `claude mcp list` that `{NAME}` is connected. Tell the person that
the `mcp__{NAME}__*` tools load in the next session: exit with `/exit` and open
Claude Code again. The skill works right away.

In other agents (Cursor, Codex and the like), add an HTTP MCP server at
`{BASE}/mcp` with the header
`Authorization: Bearer <contents of ~/.config/webo/{NAME}.token>`.
