use serde::Serialize;
use std::collections::VecDeque;

/// Uma amostra do histórico (leve: só o que vira sparkline).
#[derive(Clone, Copy, Serialize)]
pub struct Sample {
    pub ts: u64,
    pub cpu_pct: f32,
    pub mem_used: u64,
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
}

/// O retrato completo do momento — é isso que o painel (e o MCP, depois) leem.
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
    pub containers_running: Option<usize>,
    pub processes: Option<usize>,
    pub uptime_secs: u64,
}

/// Informações que não mudam (ou quase) — endpoint separado pro MCP não
/// precisar do snapshot pra responder "que máquina é essa?".
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

pub struct State {
    pub snapshot: Snapshot,
    pub system: SystemInfo,
    pub history: VecDeque<Sample>,
    pub history_cap: usize,
}

impl State {
    pub fn new(history_cap: usize) -> Self {
        Self {
            snapshot: Snapshot::default(),
            system: SystemInfo::default(),
            history: VecDeque::with_capacity(history_cap),
            history_cap,
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
