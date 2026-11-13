# webo CLI

A command line for a webo server. It talks to the server's own `/mcp`
endpoint (JSON-RPC `tools/list` and `tools/call`) with the same bearer
token the MCP uses, so every MCP tool is a subcommand and a tool added to
the server shows up here without a new release.

## Install

```sh
cargo install --path cli
```

## Config

`~/.config/webo/config` (keep it `chmod 600`), one block per server:

```ini
default = homelab

[homelab]
url = https://webo.example.com/mcp
token = webo_…

[vps]
url = https://webo-vps.example.com/mcp
token = webo_…
```

`--server <name>` (or `-s`) picks a block; without it the CLI uses
`WEBO_SERVER`, then `default`, then the first block. `WEBO_URL` and
`WEBO_TOKEN` override the chosen block when `--server` is not given.

## Commands

`webo --help` lists the commands the server offers right now;
`webo <command> --help` shows a command's parameters from its schema.

```sh
webo projects                         # list_projects
webo -s vps health                    # server_health on another server
webo status blogo                     # project_status
webo logs blogo "timeout" --level error --window 7d
webo errors blogo && webo error blogo 12
webo query blogo "select count(*) from posts"
webo env blogo                        # variables, values masked
webo tools                            # every command and its schema, as JSON
webo servers                          # configured servers, no tokens
```

Arguments map onto the tool's input schema: required properties (and an
optional `action`) are positional, in schema order, followed by one
optional free-text property; everything else is `--kebab-case` flags.
Booleans are bare flags (`--descending`, `--no-descending`), arrays are
comma-separated (`--issue-ids 3,4`), and any property can be read from a
file with `--<name>-file <path>` (`-` reads stdin).

Commands named after the MCP tools also work (`webo list-projects`,
`webo project_status blogo`).

## Writes

Anything the server does not mark read-only, and that is not a plain
`list` or a read-only query, needs `--yes`. Without it the CLI prints the
plan and exits 2; `--dry-run` prints the plan and sends nothing. With
`--yes`, a boolean `confirm` is filled in for you; `delete` still needs the
slug typed twice (`webo delete app app --yes`).

Secrets never go on the command line: `env set` reads the value from stdin
or `--value-file`, `deploy` takes variables from `--env-file .env`, and
plans mask them.

```sh
printf %s "$NEW_KEY" | webo env blogo set API_KEY --yes
webo deploy app --database managed --env-file .env --dry-run
```

## Output

Always JSON on stdout. Success is
`{"ok": true, "server": …, "tool": …, "result": …}`, where `result` is the
tool's structured answer or its text as a list of lines. Failures are
`{"ok": false, "error": {"code", "message"}}` with exit code 1 (server,
network, auth) or 2 (usage, confirmation needed).
