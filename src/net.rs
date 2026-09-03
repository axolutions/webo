//! Where to bind the MCP listener. The panel is public (behind Cloudflare
//! Access); the MCP server can operate the machine, so it must never be
//! reachable from the LAN or the internet. The Tailscale address is the whole
//! access control: binding to it means the socket does not exist anywhere else.

/// Tailscale hands out 100.64.0.0/10 (CGNAT) for IPv4.
pub fn is_tailscale_v4(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// Picks the address to bind the MCP listener to, in order:
/// an explicit `WEBO_MCP_BIND`, then the host's Tailscale IPv4, then nothing —
/// and nothing means the MCP server does not start at all. Falling back to
/// 0.0.0.0 would publish operational tools to the network; refusing to start
/// is the safe failure.
pub fn mcp_bind(port: u16, addrs: &[std::net::IpAddr]) -> Option<String> {
    if let Ok(explicit) = std::env::var("WEBO_MCP_BIND") {
        let explicit = explicit.trim().to_string();
        if !explicit.is_empty() {
            return Some(explicit);
        }
    }
    addrs
        .iter()
        .find_map(|ip| match ip {
            std::net::IpAddr::V4(v4) if is_tailscale_v4(*v4) => Some(format!("{v4}:{port}")),
            _ => None,
        })
}

/// The machine's IPv4 addresses, read straight from the kernel. Avoids pulling
/// a crate in for something `getifaddrs` answers — and on Linux the container
/// shares the host network namespace when `network_mode: host` is set, so the
/// Tailscale address is visible.
#[cfg(unix)]
pub fn local_addrs() -> Vec<std::net::IpAddr> {
    // Reads /proc/net/fib_trie-free: ask the OS through a UDP socket per
    // candidate is unreliable, so parse `ip -4 addr` when present and fall back
    // to the tailscale CLI. Both are optional; an explicit WEBO_MCP_BIND always
    // wins and needs neither.
    let mut out = Vec::new();
    for (cmd, args) in [
        ("tailscale", vec!["ip", "-4"]),
        ("ip", vec!["-4", "-o", "addr", "show"]),
    ] {
        let Ok(o) = std::process::Command::new(cmd).args(&args).output() else { continue };
        if !o.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&o.stdout);
        out.extend(parse_addrs(&text));
        if !out.is_empty() {
            break;
        }
    }
    out
}

#[cfg(not(unix))]
pub fn local_addrs() -> Vec<std::net::IpAddr> {
    Vec::new()
}

/// Pulls dotted-quad addresses out of whatever the command printed.
pub fn parse_addrs(text: &str) -> Vec<std::net::IpAddr> {
    let mut out = Vec::new();
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        if token.matches('.').count() != 3 {
            continue;
        }
        if let Ok(ip) = token.parse::<std::net::Ipv4Addr>() {
            let addr = std::net::IpAddr::V4(ip);
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn only_the_tailscale_range_counts() {
        assert!(is_tailscale_v4(Ipv4Addr::new(100, 124, 135, 53)), "the real one");
        assert!(is_tailscale_v4(Ipv4Addr::new(100, 64, 0, 1)), "start of the range");
        assert!(is_tailscale_v4(Ipv4Addr::new(100, 127, 255, 254)), "end of the range");
        // everything else is not
        assert!(!is_tailscale_v4(Ipv4Addr::new(100, 63, 255, 255)), "just below");
        assert!(!is_tailscale_v4(Ipv4Addr::new(100, 128, 0, 1)), "just above");
        assert!(!is_tailscale_v4(Ipv4Addr::new(192, 168, 1, 10)), "LAN");
        assert!(!is_tailscale_v4(Ipv4Addr::new(127, 0, 0, 1)), "loopback");
        assert!(!is_tailscale_v4(Ipv4Addr::new(8, 8, 8, 8)), "public");
    }

    #[test]
    fn the_bind_prefers_tailscale_and_refuses_to_guess() {
        let _lock = crate::testutil::env_lock();
        std::env::remove_var("WEBO_MCP_BIND");
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
            IpAddr::V4(Ipv4Addr::new(100, 124, 135, 53)),
        ];
        assert_eq!(mcp_bind(5051, &addrs).unwrap(), "100.124.135.53:5051");

        // no tailscale address: the server must NOT start rather than bind wide
        let lan_only = vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))];
        assert_eq!(
            mcp_bind(5051, &lan_only),
            None,
            "binding 0.0.0.0 would publish operational tools — refusing is the safe failure"
        );
        assert_eq!(mcp_bind(5051, &[]), None);

        // an explicit override wins, for a dev machine with no tailnet
        std::env::set_var("WEBO_MCP_BIND", "127.0.0.1:5051");
        assert_eq!(mcp_bind(5051, &lan_only).unwrap(), "127.0.0.1:5051");
        // blank is treated as unset, not as a bind to everything
        std::env::set_var("WEBO_MCP_BIND", "   ");
        assert_eq!(mcp_bind(5051, &lan_only), None);
        std::env::remove_var("WEBO_MCP_BIND");
    }

    #[test]
    fn addresses_are_pulled_out_of_command_output() {
        // `tailscale ip -4`
        assert_eq!(
            parse_addrs("100.124.135.53\n"),
            vec![IpAddr::V4(Ipv4Addr::new(100, 124, 135, 53))]
        );
        // `ip -4 -o addr show`
        let iproute = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever\n\
                       2: eth0    inet 192.168.1.10/24 brd 192.168.1.255 scope global eth0\n\
                       4: tailscale0    inet 100.124.135.53/32 scope global tailscale0\n";
        let found = parse_addrs(iproute);
        assert!(found.contains(&IpAddr::V4(Ipv4Addr::new(100, 124, 135, 53))));
        assert!(found.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert_eq!(
            mcp_bind_ignoring_env(5051, &found).unwrap(),
            "100.124.135.53:5051",
            "the tailscale one is picked out of the noise"
        );
        assert!(parse_addrs("no addresses here").is_empty());
    }

    /// mcp_bind reads the environment; this asserts the selection alone.
    fn mcp_bind_ignoring_env(port: u16, addrs: &[IpAddr]) -> Option<String> {
        addrs.iter().find_map(|ip| match ip {
            IpAddr::V4(v4) if is_tailscale_v4(*v4) => Some(format!("{v4}:{port}")),
            _ => None,
        })
    }
}
