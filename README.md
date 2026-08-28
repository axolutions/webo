# webo

A health panel for your server — CPU, memory, disk, temperature, battery and
network on a single screen, genuinely lightweight (one Rust binary, < 50 MB of
RAM), with a JSON API ready for automation and agents (MCP).

```
docker compose -f deploy/docker-compose.yml up -d --build
# panel at http://your-server:5050
```

## What it shows

- **CPU** — usage, load, 24 h sparkline
- **Memory** — used/total, sparkline
- **Disk** — used/total/free of the root filesystem
- **Temperature** — CPU, with a visual alert at 85 °C
- **Battery** — charge, status and charge limit (laptop servers)
- **Network** — download/upload rates
- **System** — OS, kernel, architecture, process count

Cards for metrics your hardware doesn't have (a battery in a datacenter, say)
simply disappear.

## API (the same contract the panel uses)

| Endpoint | Returns |
|---|---|
| `GET /api/v1/snapshot` | everything about the current moment |
| `GET /api/v1/history?minutes=1440` | CPU/RAM/network series (15 s samples, 24 h in memory) |
| `GET /api/v1/system` | machine identity (hostname, OS, kernel, hardware) |
| `GET /healthz` | `ok` |

Nothing is UI-only: everything the panel shows comes from these endpoints —
they are how an MCP server (or any automation) sees the machine.

## Configuration

| Env | Default | What |
|---|---|---|
| `WEBO_BIND` | `0.0.0.0:5050` | listen address |
| `WEBO_SAMPLE_SECS` | `15` | sampling interval |
| `WEBO_NET_DEV` | `/proc/net/dev` | network rates source (in a container, mount the host's) |
| `WEBO_HOSTNAME` | — | overrides the reported hostname |

The compose mounts (`/sys`, `/proc/net/dev`, `/etc/hostname`, `/etc/os-release`)
are optional — without them webo still works and just hides the corresponding
metrics.

## Security

webo has **no built-in authentication** (it is a read-only observer). Don't
expose port 5050 directly to the internet: put an authenticating proxy,
Cloudflare Access, or a VPN/private network in front of it.

## License

MIT
