use crate::metrics::{ProcessChild, ProcessGroup, Snapshot, State, SystemInfo};
use std::collections::HashMap;
use std::fs;
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
    if let Ok(text) = fs::read_to_string("/host/etc/os-release") {
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                return name.trim_matches('"').to_string();
            }
        }
    }
    System::long_os_version().unwrap_or_default()
}

/// Path to /proc/net/dev. Inside a container, mount the host's and point
/// WEBO_NET_DEV at it; otherwise rates reflect the container only.
fn net_dev_path() -> String {
    std::env::var("WEBO_NET_DEV").unwrap_or_else(|_| "/proc/net/dev".into())
}

/// Sum rx/tx bytes across all interfaces except lo/veth/br-/docker.
fn read_net_totals() -> Option<(u64, u64)> {
    let text = fs::read_to_string(net_dev_path()).ok()?;
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in text.lines().skip(2) {
        let (name, rest) = line.split_once(':')?;
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
    Some((rx, tx))
}

fn read_battery() -> (Option<u8>, Option<u8>, Option<String>) {
    let base = std::path::Path::new("/sys/class/power_supply");
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

        let Ok(stat) = fs::read_to_string(base.join("stat")) else { continue };
        // comm is inside parens and may contain spaces: split around the last ')'
        let Some(open) = stat.find('(') else { continue };
        let Some(close) = stat.rfind(')') else { continue };
        let comm = stat[open + 1..close].to_string();
        let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        // after the state field: ppid=rest[1], utime=rest[11], stime=rest[12], starttime=rest[19]
        if rest.len() < 20 {
            continue;
        }
        let ppid: u32 = rest[1].parse().unwrap_or(0);
        let utime: u64 = rest[11].parse().unwrap_or(0);
        let stime: u64 = rest[12].parse().unwrap_or(0);
        let starttime: u64 = rest[19].parse().unwrap_or(0);
        let cpu_ticks = utime + stime;

        let mem_bytes = fs::read_to_string(base.join("statm"))
            .ok()
            .and_then(|t| t.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
            .map(|pages| pages * PAGE_SIZE)
            .unwrap_or(0);

        // may be unreadable for some processes — treat as zero
        let io_bytes = fs::read_to_string(base.join("io"))
            .map(|t| {
                t.lines()
                    .filter_map(|l| {
                        l.strip_prefix("read_bytes: ")
                            .or_else(|| l.strip_prefix("write_bytes: "))
                            .and_then(|v| v.trim().parse::<u64>().ok())
                    })
                    .sum::<u64>()
            })
            .unwrap_or(0);

        let (cpu_pct, disk_bps) = match prev.get(&pid) {
            Some(p) => (
                (cpu_ticks.saturating_sub(p.cpu_ticks) as f32 / CLK_TCK as f32)
                    / sample_secs.max(1) as f32
                    * 100.0,
                io_bytes.saturating_sub(p.io_bytes) / sample_secs.max(1),
            ),
            None => (0.0, 0),
        };
        seen.insert(pid, ProcSample { cpu_ticks, io_bytes });

        let uptime_secs = (host_uptime - (starttime as f64 / CLK_TCK as f64)).max(0.0) as u64;
        list.push(RawProc {
            pid,
            ppid,
            comm,
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

    let mut groups: HashMap<u32, ProcessGroup> = HashMap::new();
    let mut members: HashMap<u32, Vec<usize>> = HashMap::new();
    for i in 0..raw.len() {
        let r = root_of(i);
        members.entry(raw[r].pid).or_default().push(i);
    }

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
        groups.insert(root_pid, g);
    }

    let mut list: Vec<ProcessGroup> = groups.into_values().collect();
    list.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(b.mem_bytes.cmp(&a.mem_bytes)));
    list.truncate(40);
    list
}

fn root_disk(disks: &mut Disks) -> (u64, u64) {
    disks.refresh(true);
    // largest filesystem mounted at "/" (in a container, overlayfs reflects the host disk)
    let mut best = (0u64, 0u64);
    for d in disks.iter() {
        if d.mount_point() == std::path::Path::new("/") && d.total_space() > best.1 {
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
        if let Some(t) = read_net_totals() {
            last_net = Some(t);
        }

        let (disk_used, disk_total) = root_disk(&mut disks);
        let (battery_pct, battery_limit_pct, battery_status) = read_battery();
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
