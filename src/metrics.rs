use serde::Serialize;
use std::collections::VecDeque;

/// One history sample (lightweight: just what becomes a sparkline).
#[derive(Clone, Copy, Serialize)]
pub struct Sample {
    pub ts: u64,
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
}

/// The full picture of the current moment — this is what the panel
/// (and, later, the MCP server) read.
#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub ts: u64,
    pub cpu_pct: f32,
    pub load_1m: f64,
    pub cpu_threads: usize,
    pub cpu_brand: String,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    pub temp_c: Option<f32>,
    pub battery_pct: Option<u8>,
    pub battery_limit_pct: Option<u8>,
    pub battery_status: Option<String>,
    pub processes: Option<usize>,
    pub uptime_secs: u64,
}

/// A member of a process group (subprocess), as shown when a row expands.
#[derive(Clone, Serialize)]
pub struct ProcessChild {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub disk_bps: u64,
    pub uptime_secs: u64,
}

/// One row of the Processes tab: an app and its subprocess tree, aggregated.
/// Grouping is by parent chain sharing the same executable (argv0) or the
/// same comm — how browsers, postgres workers etc. spawn. `kind` is only
/// what is honestly detectable from the binary name (technology, never purpose).
#[derive(Clone, Serialize)]
pub struct ProcessGroup {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    pub kind: String,
    pub uptime_secs: u64,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub disk_bps: u64,
    pub procs: usize,
    pub children: Vec<ProcessChild>,
}

/// Facts that rarely change — a separate endpoint so automations can answer
/// "what machine is this?" without a snapshot.
#[derive(Clone, Default, Serialize)]
pub struct SystemInfo {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub cpu_brand: String,
    pub cpu_threads: usize,
    pub mem_total: u64,
    pub disk_total: u64,
    pub webo_version: String,
    pub sample_secs: u64,
}

/// Live view of one container belonging to a project.
#[derive(Clone, Serialize)]
pub struct ProjectContainer {
    pub name: String,
    pub image: String,
    pub state: String,
    pub uptime_secs: u64,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub disk_bps: u64,
}

#[derive(Clone, Copy, Serialize)]
pub struct ProjectSample {
    pub ts: u64,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
}

/// Live (in-memory) aggregate for a project: containers + 24 h history.
#[derive(Clone, Default, Serialize)]
pub struct ProjectLive {
    pub containers: Vec<ProjectContainer>,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub disk_bps: u64,
    pub image_bytes: u64,
    pub volume_bytes: u64,
    pub history: VecDeque<ProjectSample>,
}

pub struct State {
    pub snapshot: Snapshot,
    pub system: SystemInfo,
    pub history: VecDeque<Sample>,
    pub history_cap: usize,
    pub processes: Vec<ProcessGroup>,
    pub projects_live: std::collections::HashMap<String, ProjectLive>,
}

impl State {
    pub fn new(history_cap: usize) -> Self {
        Self {
            snapshot: Snapshot::default(),
            system: SystemInfo::default(),
            history: VecDeque::with_capacity(history_cap),
            history_cap,
            processes: Vec::new(),
            projects_live: std::collections::HashMap::new(),
        }
    }

    pub fn push(&mut self, snap: Snapshot) {
        if self.history.len() == self.history_cap {
            self.history.pop_front();
        }
        self.history.push_back(Sample {
            ts: snap.ts,
            cpu_pct: snap.cpu_pct,
            mem_used: snap.mem_used,
            net_rx_bps: snap.net_rx_bps,
            net_tx_bps: snap.net_tx_bps,
        });
        self.snapshot = snap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ts: u64) -> Snapshot {
        Snapshot { ts, cpu_pct: 1.0, mem_used: 42, ..Default::default() }
    }

    #[test]
    fn push_appends_history_and_updates_snapshot() {
        let mut st = State::new(3);
        st.push(snap(1));
        st.push(snap(2));
        assert_eq!(st.history.len(), 2);
        assert_eq!(st.snapshot.ts, 2);
        assert_eq!(st.history.back().unwrap().ts, 2);
        assert_eq!(st.history.back().unwrap().mem_used, 42);
    }

    #[test]
    fn push_evicts_the_oldest_at_capacity() {
        let mut st = State::new(3);
        for ts in 1..=5 {
            st.push(snap(ts));
        }
        assert_eq!(st.history.len(), 3);
        assert_eq!(st.history.front().unwrap().ts, 3);
        assert_eq!(st.history.back().unwrap().ts, 5);
    }

    #[test]
    fn new_state_is_empty() {
        let st = State::new(8);
        assert!(st.history.is_empty());
        assert!(st.processes.is_empty());
        assert_eq!(st.snapshot.ts, 0);
    }
}
