use crate::metrics::{ProcessChild, ProcessGroup, Snapshot, State, SystemInfo};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Components, Disks, System};
use tokio::sync::RwLock;

fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Inside a container, hostname/os belong to the container; the optional
/// /host/etc/* mounts (see compose) surface the host's instead.
fn host_hostname() -> String {
    if let Ok(h) = std::env::var("WEBO_HOSTNAME") {
        return h;
    }
    if let Ok(h) = fs::read_to_string("/host/etc/hostname") {
        return h.trim().to_string();
    }
    System::host_name().unwrap_or_default()
}

fn host_os() -> String {
    fs::read_to_string("/host/etc/os-release")
        .ok()
        .and_then(|text| parse_pretty_name(&text))
        .unwrap_or_else(|| System::long_os_version().unwrap_or_default())
}

fn parse_pretty_name(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        line.strip_prefix("PRETTY_NAME=")
            .map(|name| name.trim_matches('"').to_string())
    })
}

/// Path to /proc/net/dev. Inside a container, mount the host's and point
/// WEBO_NET_DEV at it; otherwise rates reflect the container only.
fn net_dev_path() -> String {
    std::env::var("WEBO_NET_DEV").unwrap_or_else(|_| "/proc/net/dev".into())
}

fn read_net_totals() -> Option<(u64, u64)> {
    fs::read_to_string(net_dev_path()).ok().map(|t| parse_net_dev(&t))
}

/// Sum rx/tx bytes across all interfaces except lo/veth/br-/docker.
fn parse_net_dev(text: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name == "lo" || name.starts_with("veth") || name.starts_with("br-") || name.starts_with("docker") {
            continue;
        }
        let cols: Vec<&str> = rest.split_whitespace().collect();
        if cols.len() >= 9 {
            rx += cols[0].parse::<u64>().unwrap_or(0);
            tx += cols[8].parse::<u64>().unwrap_or(0);
        }
    }
    (rx, tx)
}

fn read_battery(base: &Path) -> (Option<u8>, Option<u8>, Option<String>) {
    let Ok(entries) = fs::read_dir(base) else { return (None, None, None) };
    for e in entries.flatten() {
        let p = e.path();
        if !e.file_name().to_string_lossy().starts_with("BAT") {
            continue;
        }
        let read_u8 = |f: &str| fs::read_to_string(p.join(f)).ok()?.trim().parse::<u8>().ok();
        let pct = read_u8("capacity");
        let limit = read_u8("charge_control_end_threshold");
        let status = fs::read_to_string(p.join("status")).ok().map(|s| s.trim().to_string());
        return (pct, limit, status);
    }
    (None, None, None)
}

fn read_temperature(components: &mut Components) -> Option<f32> {
    components.refresh(true);
    let mut cpu_temp: Option<f32> = None;
    let mut max_temp: Option<f32> = None;
    for c in components.iter() {
        let t = c.temperature()?;
        let label = c.label().to_lowercase();
        if label.contains("package") || label.contains("tctl") || label.contains("cpu") || label.contains("core") {
            cpu_temp = Some(cpu_temp.map_or(t, |m: f32| m.max(t)));
        }
        max_temp = Some(max_temp.map_or(t, |m: f32| m.max(t)));
    }
    cpu_temp.or(max_temp)
}

fn count_processes() -> Option<usize> {
    let entries = fs::read_dir("/proc").ok()?;
    Some(
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
            .count(),
    )
}

/// Technology detectable from the binary name — never a guess at purpose.
fn kind_for(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n == "node" || n == "nodejs" || n == "bun" || n == "deno" { "node" }
    else if n.starts_with("postgres") { "postgres" }
    else if n == "dockerd" || n == "docker-proxy" || n.starts_with("containerd") || n.starts_with("runc") { "docker" }
    else if n == "cloudflared" { "cloudflare" }
    else if n.starts_with("redis") { "redis" }
    else if n.starts_with("python") { "python" }
    else if n == "nginx" || n == "caddy" || n == "httpd" || n == "traefik" { "web" }
    else if n.starts_with("mysql") || n.starts_with("mariadb") { "mysql" }
    else if n == "java" { "java" }
    else if n == "ruby" || n.starts_with("puma") || n.starts_with("sidekiq") { "ruby" }
    else if n == "bash" || n == "zsh" || n == "sh" || n == "fish" { "shell" }
    else if n == "sshd" || n == "ssh" { "ssh" }
    else if n.starts_with("systemd") { "systemd" }
    else if n == "webo" { "webo" }
    else { "generic" }
}

/// Manual /proc scanner. sysinfo misbehaves under `pid: host` inside a
/// container, and /proc is all we need: stat (cpu ticks, ppid, start time),
/// statm (rss), cmdline, io (bytes). CPU% is a delta between ticks,
/// like top(1) — % of a single core.
const CLK_TCK: u64 = 100;
const PAGE_SIZE: u64 = 4096;

fn read_host_uptime() -> f64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

struct ProcSample {
    cpu_ticks: u64,
    io_bytes: u64,
}

struct RawProc {
    pid: u32,
    ppid: u32,
    comm: String,
    /// First whitespace token of argv[0] — the executable path. Some apps
    /// (Firefox content processes) rewrite their cmdline into one big string,
    /// so raw argv[0] comparison would never match the parent.
    bin: String,
    cmd: String,
    uptime_secs: u64,
    cpu_pct: f32,
    mem_bytes: u64,
    disk_bps: u64,
}

struct StatFields {
    comm: String,
    ppid: u32,
    cpu_ticks: u64,
    starttime: u64,
}

/// comm is inside parens and may contain spaces or parens itself:
/// split around the LAST ')'.
fn parse_stat(stat: &str) -> Option<StatFields> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    // after the state field: ppid=rest[1], utime=rest[11], stime=rest[12], starttime=rest[19]
    if rest.len() < 20 {
        return None;
    }
    let utime: u64 = rest[11].parse().unwrap_or(0);
    let stime: u64 = rest[12].parse().unwrap_or(0);
    Some(StatFields {
        comm,
        ppid: rest[1].parse().unwrap_or(0),
        cpu_ticks: utime + stime,
        starttime: rest[19].parse().unwrap_or(0),
    })
}

/// RSS in bytes from /proc/<pid>/statm (second field, in pages).
fn parse_statm(text: &str) -> Option<u64> {
    text.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<u64>().ok())
        .map(|pages| pages * PAGE_SIZE)
}

/// read_bytes + write_bytes from /proc/<pid>/io.
fn parse_io(text: &str) -> u64 {
    text.lines()
        .filter_map(|l| {
            l.strip_prefix("read_bytes: ")
                .or_else(|| l.strip_prefix("write_bytes: "))
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
        .sum()
}

fn scan_procs(prev: &mut HashMap<u32, ProcSample>, sample_secs: u64) -> Vec<RawProc> {
    let host_uptime = read_host_uptime();
    let mut seen: HashMap<u32, ProcSample> = HashMap::new();
    let mut list: Vec<RawProc> = Vec::new();

    let Ok(entries) = fs::read_dir("/proc") else { return list };
    for e in entries.flatten() {
        let fname = e.file_name();
        let Ok(pid) = fname.to_string_lossy().parse::<u32>() else { continue };
        let base = e.path();

        // kernel threads have an empty cmdline — skip them
        let cmdline = fs::read(base.join("cmdline")).unwrap_or_default();
        if cmdline.is_empty() {
            continue;
        }
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect();
        let bin = args
            .first()
            .and_then(|a| a.split_whitespace().next())
            .unwrap_or_default()
            .to_string();
        let cmd: String = args.join(" ").chars().take(200).collect();

        let Ok(stat_text) = fs::read_to_string(base.join("stat")) else { continue };
        let Some(st) = parse_stat(&stat_text) else { continue };

        let mem_bytes = fs::read_to_string(base.join("statm"))
            .ok()
            .and_then(|t| parse_statm(&t))
            .unwrap_or(0);

        // may be unreadable for some processes — treat as zero
        let io_bytes = fs::read_to_string(base.join("io")).map(|t| parse_io(&t)).unwrap_or(0);

        let (cpu_pct, disk_bps) = match prev.get(&pid) {
            Some(p) => (
                (st.cpu_ticks.saturating_sub(p.cpu_ticks) as f32 / CLK_TCK as f32)
                    / sample_secs.max(1) as f32
                    * 100.0,
                io_bytes.saturating_sub(p.io_bytes) / sample_secs.max(1),
            ),
            None => (0.0, 0),
        };
        seen.insert(pid, ProcSample { cpu_ticks: st.cpu_ticks, io_bytes });

        let uptime_secs = (host_uptime - (st.starttime as f64 / CLK_TCK as f64)).max(0.0) as u64;
        list.push(RawProc {
            pid,
            ppid: st.ppid,
            comm: st.comm,
            bin,
            cmd,
            uptime_secs,
            cpu_pct,
            mem_bytes,
            disk_bps,
        });
    }

    *prev = seen;
    list
}

/// Groups an app with its subprocess tree: a process joins its parent's group
/// while the parent runs the same executable or has the same comm — the way
/// browsers spawn content processes and postgres spawns workers.
fn group_processes(raw: Vec<RawProc>) -> Vec<ProcessGroup> {
    let by_pid: HashMap<u32, usize> = raw.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();

    let root_of = |start: usize| -> usize {
        let mut cur = start;
        for _ in 0..64 {
            let me = &raw[cur];
            let Some(&pi) = by_pid.get(&me.ppid) else { break };
            let parent = &raw[pi];
            let same_bin = !me.bin.is_empty() && parent.bin == me.bin;
            if same_bin || parent.comm == me.comm {
                cur = pi;
            } else {
                break;
            }
        }
        cur
    };

    let mut members: HashMap<u32, Vec<usize>> = HashMap::new();
    for i in 0..raw.len() {
        let r = root_of(i);
        members.entry(raw[r].pid).or_default().push(i);
    }

    let mut list: Vec<ProcessGroup> = Vec::new();
    for (root_pid, idxs) in members {
        let root = &raw[by_pid[&root_pid]];
        let mut g = ProcessGroup {
            pid: root.pid,
            name: root.comm.clone(),
            cmd: root.cmd.clone(),
            kind: kind_for(&root.comm).to_string(),
            uptime_secs: root.uptime_secs,
            cpu_pct: 0.0,
            mem_bytes: 0,
            disk_bps: 0,
            procs: idxs.len(),
            children: Vec::new(),
        };
        for &i in &idxs {
            let p = &raw[i];
            g.cpu_pct += p.cpu_pct;
            g.mem_bytes += p.mem_bytes;
            g.disk_bps += p.disk_bps;
            if p.pid != root_pid {
                g.children.push(ProcessChild {
                    pid: p.pid,
                    name: p.comm.clone(),
                    cpu_pct: p.cpu_pct,
                    mem_bytes: p.mem_bytes,
                    disk_bps: p.disk_bps,
                    uptime_secs: p.uptime_secs,
                });
            }
        }
        g.children.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(b.mem_bytes.cmp(&a.mem_bytes)));
        g.children.truncate(20);
        list.push(g);
    }

    list.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(b.mem_bytes.cmp(&a.mem_bytes)));
    list.truncate(40);
    list
}

fn root_disk(disks: &mut Disks) -> (u64, u64) {
    disks.refresh(true);
    // largest filesystem mounted at "/" (in a container, overlayfs reflects the host disk)
    let mut best = (0u64, 0u64);
    for d in disks.iter() {
        if d.mount_point() == Path::new("/") && d.total_space() > best.1 {
            best = (d.total_space() - d.available_space(), d.total_space());
        }
    }
    if best.1 == 0 {
        for d in disks.iter() {
            if d.total_space() > best.1 {
                best = (d.total_space() - d.available_space(), d.total_space());
            }
        }
    }
    best
}

pub async fn run(state: Arc<RwLock<State>>, sample_secs: u64) {
    let mut sys = System::new();
    let mut disks = Disks::new_with_refreshed_list();
    let mut components = Components::new_with_refreshed_list();
    let mut last_net: Option<(u64, u64)> = None;
    let mut proc_prev: HashMap<u32, ProcSample> = HashMap::new();

    sys.refresh_cpu_usage();
    sys.refresh_memory();

    // static facts
    {
        let cpu_brand = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
        let (_, disk_total) = root_disk(&mut disks);
        let mut st = state.write().await;
        st.system = SystemInfo {
            hostname: host_hostname(),
            os: host_os(),
            kernel: System::kernel_version().unwrap_or_default(),
            arch: System::cpu_arch(),
            cpu_brand,
            cpu_threads: sys.cpus().len(),
            mem_total: sys.total_memory(),
            disk_total,
            webo_version: env!("CARGO_PKG_VERSION").to_string(),
            sample_secs,
        };
    }

    let mut tick = tokio::time::interval(Duration::from_secs(sample_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        sys.refresh_cpu_usage();
        sys.refresh_memory();

        let (rx_bps, tx_bps) = match (read_net_totals(), last_net) {
            (Some((rx, tx)), Some((prx, ptx))) => (
                rx.saturating_sub(prx) / sample_secs,
                tx.saturating_sub(ptx) / sample_secs,
            ),
            _ => (0, 0),
        };
        if let Some(totals) = read_net_totals() {
            last_net = Some(totals);
        }

        let (disk_used, disk_total) = root_disk(&mut disks);
        let (battery_pct, battery_limit_pct, battery_status) =
            read_battery(Path::new("/sys/class/power_supply"));
        let procs = group_processes(scan_procs(&mut proc_prev, sample_secs));

        let snap = Snapshot {
            ts: now_ts(),
            cpu_pct: sys.global_cpu_usage(),
            load_1m: System::load_average().one,
            cpu_threads: sys.cpus().len(),
            cpu_brand: sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default(),
            mem_used: sys.used_memory(),
            mem_total: sys.total_memory(),
            swap_used: sys.used_swap(),
            swap_total: sys.total_swap(),
            disk_used,
            disk_total,
            net_rx_bps: rx_bps,
            net_tx_bps: tx_bps,
            temp_c: read_temperature(&mut components),
            battery_pct,
            battery_limit_pct,
            battery_status,
            processes: count_processes(),
            uptime_secs: System::uptime(),
        };

        let mut st = state.write().await;
        st.processes = procs;
        st.push(snap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pid: u32, ppid: u32, comm: &str, bin: &str, cpu: f32, mem: u64) -> RawProc {
        RawProc {
            pid,
            ppid,
            comm: comm.into(),
            bin: bin.into(),
            cmd: format!("{bin} --flag"),
            uptime_secs: 100,
            cpu_pct: cpu,
            mem_bytes: mem,
            disk_bps: 1,
        }
    }

    #[test]
    fn kind_for_detects_known_binaries() {
        assert_eq!(kind_for("node"), "node");
        assert_eq!(kind_for("postgres"), "postgres");
        assert_eq!(kind_for("dockerd"), "docker");
        assert_eq!(kind_for("containerd-shim"), "docker");
        assert_eq!(kind_for("cloudflared"), "cloudflare");
        assert_eq!(kind_for("redis-server"), "redis");
        assert_eq!(kind_for("python3"), "python");
        assert_eq!(kind_for("nginx"), "web");
        assert_eq!(kind_for("mysqld"), "mysql");
        assert_eq!(kind_for("java"), "java");
        assert_eq!(kind_for("bash"), "shell");
        assert_eq!(kind_for("sshd"), "ssh");
        assert_eq!(kind_for("systemd-journald"), "systemd");
        assert_eq!(kind_for("webo"), "webo");
        assert_eq!(kind_for("codo"), "generic");
        assert_eq!(kind_for("NODE"), "node");
    }

    #[test]
    fn parse_stat_handles_plain_comm() {
        let line = "42 (webo) S 1 42 42 0 -1 4194304 100 0 0 0 250 50 0 0 20 0 4 0 12345 1000 200 18446744073709551615";
        let f = parse_stat(line).unwrap();
        assert_eq!(f.comm, "webo");
        assert_eq!(f.ppid, 1);
        assert_eq!(f.cpu_ticks, 300);
        assert_eq!(f.starttime, 12345);
    }

    #[test]
    fn parse_stat_handles_spaces_and_parens_in_comm() {
        let line = "99 (Isolated Web (Co)) R 42 99 99 0 -1 0 0 0 0 0 10 20 0 0 20 0 1 0 777 0 0 0";
        let f = parse_stat(line).unwrap();
        assert_eq!(f.comm, "Isolated Web (Co)");
        assert_eq!(f.ppid, 42);
        assert_eq!(f.cpu_ticks, 30);
        assert_eq!(f.starttime, 777);
    }

    #[test]
    fn parse_stat_rejects_garbage() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("1 (short) S 0 1").is_none());
    }

    #[test]
    fn parse_statm_returns_rss_bytes() {
        assert_eq!(parse_statm("999 250 30 10 0 200 0"), Some(250 * PAGE_SIZE));
        assert_eq!(parse_statm(""), None);
    }

    #[test]
    fn parse_io_sums_read_and_write() {
        let text = "rchar: 1\nwchar: 2\nread_bytes: 1000\nwrite_bytes: 234\ncancelled_write_bytes: 9\n";
        assert_eq!(parse_io(text), 1234);
        assert_eq!(parse_io(""), 0);
    }

    #[test]
    fn parse_net_dev_sums_and_filters_virtual_interfaces() {
        let text = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:   999      10    0    0    0     0          0         0      999      10    0    0    0     0       0          0
  eth0:  1000      10    0    0    0     0          0         0      500       5    0    0    0     0       0          0
 veth1:  7777      10    0    0    0     0          0         0     7777      10    0    0    0     0       0          0
 wlan0:   200       2    0    0    0     0          0         0      100       1    0    0    0     0       0          0
";
        assert_eq!(parse_net_dev(text), (1200, 600));
    }

    #[test]
    fn parse_pretty_name_reads_os_release() {
        let text = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.2 LTS\"\nID=ubuntu\n";
        assert_eq!(parse_pretty_name(text).as_deref(), Some("Ubuntu 24.04.2 LTS"));
        assert_eq!(parse_pretty_name("NAME=x\n"), None);
    }

    #[test]
    fn battery_reads_bat_directory() {
        let dir = std::env::temp_dir().join(format!("webo-bat-test-{}", std::process::id()));
        let bat = dir.join("BAT0");
        fs::create_dir_all(&bat).unwrap();
        fs::write(bat.join("capacity"), "80\n").unwrap();
        fs::write(bat.join("charge_control_end_threshold"), "80\n").unwrap();
        fs::write(bat.join("status"), "Not charging\n").unwrap();
        let (pct, limit, status) = read_battery(&dir);
        assert_eq!(pct, Some(80));
        assert_eq!(limit, Some(80));
        assert_eq!(status.as_deref(), Some("Not charging"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn battery_absent_is_all_none() {
        let (pct, limit, status) = read_battery(Path::new("/nonexistent-webo-test"));
        assert!(pct.is_none() && limit.is_none() && status.is_none());
    }

    #[test]
    fn grouping_collapses_a_browser_tree() {
        // firefox spawns forkserver (same bin), which spawns content processes
        // whose cmdline was rewritten into one big string — bin token still matches.
        let ff = "/snap/firefox/1/usr/lib/firefox/firefox";
        let raws = vec![
            raw(100, 1, "firefox", ff, 1.0, 500),
            raw(101, 100, "forkserver", ff, 0.1, 50),
            raw(102, 101, "Isolated Web Co", ff, 2.0, 300),
            raw(103, 101, "Isolated Web Co", ff, 0.5, 200),
            raw(200, 1, "gnome-shell", "/usr/bin/gnome-shell", 0.4, 300),
        ];
        let groups = group_processes(raws);
        assert_eq!(groups.len(), 2);
        let firefox = groups.iter().find(|g| g.name == "firefox").unwrap();
        assert_eq!(firefox.procs, 4);
        assert_eq!(firefox.children.len(), 3);
        assert!((firefox.cpu_pct - 3.6).abs() < 1e-4);
        assert_eq!(firefox.mem_bytes, 1050);
        // children sorted by cpu desc
        assert_eq!(firefox.children[0].pid, 102);
    }

    #[test]
    fn grouping_uses_comm_when_binaries_differ() {
        // postgres workers rewrite argv0 entirely but keep comm == "postgres"
        let raws = vec![
            raw(10, 1, "postgres", "/usr/lib/postgresql/16/bin/postgres", 0.2, 100),
            raw(11, 10, "postgres", "postgres:", 0.1, 40),
            raw(12, 10, "postgres", "postgres:", 0.3, 60),
        ];
        let groups = group_processes(raws);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].procs, 3);
        assert_eq!(groups[0].kind, "postgres");
    }

    #[test]
    fn grouping_keeps_unrelated_processes_apart() {
        let raws = vec![
            raw(1, 0, "systemd", "/usr/lib/systemd/systemd", 0.1, 10),
            raw(20, 1, "webo", "/usr/local/bin/webo", 0.2, 30),
            raw(21, 1, "codo", "/usr/local/bin/codo", 0.3, 40),
        ];
        let groups = group_processes(raws);
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn grouping_sorts_by_cpu_and_truncates() {
        let mut raws: Vec<RawProc> = (0..60)
            .map(|i| raw(1000 + i, 1, &format!("p{i}"), &format!("/bin/p{i}"), i as f32 / 10.0, 1))
            .collect();
        raws.push(raw(5000, 1, "hot", "/bin/hot", 99.0, 1));
        let groups = group_processes(raws);
        assert_eq!(groups.len(), 40);
        assert_eq!(groups[0].name, "hot");
    }

    #[cfg(target_os = "linux")]
    mod linux_live {
        use super::*;
        use crate::metrics::State;

        #[test]
        fn scan_procs_sees_this_process() {
            let mut prev = HashMap::new();
            let list = scan_procs(&mut prev, 1);
            assert!(!list.is_empty());
            let me = std::process::id();
            assert!(list.iter().any(|p| p.pid == me));
            // second pass computes deltas without panicking
            let list2 = scan_procs(&mut prev, 1);
            assert!(!list2.is_empty());
        }

        #[test]
        fn count_processes_counts_something() {
            assert!(count_processes().unwrap() > 0);
        }

        #[test]
        fn net_totals_read_from_proc() {
            assert!(read_net_totals().is_some());
        }

        #[test]
        fn host_uptime_is_positive() {
            assert!(read_host_uptime() > 0.0);
        }

        #[test]
        fn root_disk_finds_a_filesystem() {
            let mut disks = Disks::new_with_refreshed_list();
            let (_, total) = root_disk(&mut disks);
            assert!(total > 0);
        }

        #[test]
        fn temperature_does_not_panic() {
            let mut components = Components::new_with_refreshed_list();
            let _ = read_temperature(&mut components);
        }

        #[test]
        fn host_identity_is_readable() {
            assert!(!host_hostname().is_empty());
            assert!(!host_os().is_empty());
        }

        #[tokio::test]
        async fn run_fills_the_state_after_one_tick() {
            let state = Arc::new(RwLock::new(State::new(10)));
            let handle = tokio::spawn(run(state.clone(), 1));
            tokio::time::sleep(Duration::from_millis(1500)).await;
            handle.abort();
            let st = state.read().await;
            assert!(st.snapshot.ts > 0);
            assert!(st.system.cpu_threads > 0);
            assert!(!st.history.is_empty());
        }
    }
}
