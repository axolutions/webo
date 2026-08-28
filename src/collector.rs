use crate::metrics::{Snapshot, State, SystemInfo};
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

        state.write().await.push(snap);
    }
}
