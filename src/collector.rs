use crate::metrics::{Snapshot, State, SystemInfo};
use std::fs;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Components, Disks, System};
use tokio::sync::RwLock;

fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Caminho do /proc/net/dev. Em container, monte o do host e aponte
/// WEBO_NET_DEV pra ele; sem isso, as taxas refletem só o container.
fn net_dev_path() -> String {
    std::env::var("WEBO_NET_DEV").unwrap_or_else(|_| "/proc/net/dev".into())
}

/// Soma bytes rx/tx de todas as interfaces exceto lo/veth/br-/docker.
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

async fn count_containers() -> Option<usize> {
    let docker = bollard::Docker::connect_with_unix_defaults().ok()?;
    let opts = bollard::container::ListContainersOptions::<String>::default();
    let list = docker.list_containers(Some(opts)).await.ok()?;
    Some(list.len())
}

fn root_disk(disks: &mut Disks) -> (u64, u64) {
    disks.refresh(true);
    // maior filesystem montado em "/" (em container, o overlay reflete o disco do host)
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

    // info estática
    {
        let cpu_brand = sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default();
        let (_, disk_total) = root_disk(&mut disks);
        let mut st = state.write().await;
        st.system = SystemInfo {
            hostname: System::host_name().unwrap_or_default(),
            os: System::long_os_version().unwrap_or_default(),
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
            containers_running: count_containers().await,
            processes: count_processes(),
            uptime_secs: System::uptime(),
        };

        state.write().await.push(snap);
    }
}
