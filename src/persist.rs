//! The 7-day window. Live history is RAM only and dies with the process,
//! so every few minutes one aggregated point per scope is written to SQLite:
//! "server" for the machine, "project:<slug>" per project. Charts longer
//! than 24 h read from here — and survive webo's own deploys.

use crate::metrics::{ProjectSample, Sample, State};
use crate::store::{Store, StoredSample};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub const KEEP_SECS: i64 = 7 * 24 * 3600;

/// Averages the machine samples taken at or after `since` into one point.
pub fn average_server(history: &VecDeque<Sample>, since: u64) -> Option<StoredSample> {
    let recent: Vec<&Sample> = history.iter().filter(|s| s.ts >= since).collect();
    let n = recent.len() as u64;
    if n == 0 {
        return None;
    }
    Some(StoredSample {
        ts: recent.last()?.ts as i64,
        cpu_pct: recent.iter().map(|s| s.cpu_pct as f64).sum::<f64>() / n as f64,
        mem_bytes: (recent.iter().map(|s| s.mem_used).sum::<u64>() / n) as i64,
        disk_bps: 0,
        net_rx_bps: (recent.iter().map(|s| s.net_rx_bps).sum::<u64>() / n) as i64,
        net_tx_bps: (recent.iter().map(|s| s.net_tx_bps).sum::<u64>() / n) as i64,
    })
}

/// Same, for one project's samples.
pub fn average_project(history: &VecDeque<ProjectSample>, since: u64) -> Option<StoredSample> {
    let recent: Vec<&ProjectSample> = history.iter().filter(|s| s.ts >= since).collect();
    let n = recent.len() as u64;
    if n == 0 {
        return None;
    }
    Some(StoredSample {
        ts: recent.last()?.ts as i64,
        cpu_pct: recent.iter().map(|s| s.cpu_pct as f64).sum::<f64>() / n as f64,
        mem_bytes: (recent.iter().map(|s| s.mem_bytes).sum::<u64>() / n) as i64,
        disk_bps: (recent.iter().map(|s| s.disk_bps).sum::<u64>() / n) as i64,
        net_rx_bps: 0,
        net_tx_bps: 0,
    })
}

/// One pass: aggregate the last `window_secs` of RAM history into stored
/// points, then drop what fell out of the 7 days.
pub async fn persist_once(state: &RwLock<State>, store: &Store, window_secs: u64) {
    let st = state.read().await;
    let now = st.snapshot.ts;
    if now == 0 {
        return; // collector has not produced anything yet
    }
    let since = now.saturating_sub(window_secs);
    if let Some(s) = average_server(&st.history, since) {
        let _ = store.insert_sample("server", &s);
    }
    for (slug, live) in &st.projects_live {
        if let Some(s) = average_project(&live.history, since) {
            let _ = store.insert_sample(&format!("project:{slug}"), &s);
        }
    }
    drop(st);
    let _ = store.prune_samples(now as i64 - KEEP_SECS);
}

pub async fn run(state: Arc<RwLock<State>>, store: Arc<Store>, every_secs: u64) {
    let mut tick = tokio::time::interval(Duration::from_secs(every_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // the interval's immediate first fire — nothing to save yet
    loop {
        tick.tick().await;
        persist_once(&state, &store, every_secs).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::ProjectLive;

    #[test]
    fn averages_only_the_window_and_stamps_the_newest_ts() {
        let mut h: VecDeque<Sample> = VecDeque::new();
        for (ts, cpu) in [(100u64, 2.0f32), (200, 4.0), (300, 6.0)] {
            h.push_back(Sample { ts, cpu_pct: cpu, mem_used: ts * 10, net_rx_bps: 100, net_tx_bps: 50 });
        }
        let s = average_server(&h, 200).unwrap();
        assert_eq!(s.ts, 300);
        assert!((s.cpu_pct - 5.0).abs() < 1e-9, "only the last two points");
        assert_eq!(s.mem_bytes, 2500);
        assert_eq!(s.net_rx_bps, 100);
        assert!(average_server(&h, 999).is_none(), "empty window stores nothing");
    }

    #[test]
    fn project_average_carries_disk_instead_of_net() {
        let mut h: VecDeque<ProjectSample> = VecDeque::new();
        h.push_back(ProjectSample { ts: 10, cpu_pct: 1.0, mem_bytes: 100, disk_bps: 30 });
        h.push_back(ProjectSample { ts: 20, cpu_pct: 3.0, mem_bytes: 300, disk_bps: 10 });
        let s = average_project(&h, 0).unwrap();
        assert_eq!(s.ts, 20);
        assert!((s.cpu_pct - 2.0).abs() < 1e-9);
        assert_eq!(s.mem_bytes, 200);
        assert_eq!(s.disk_bps, 20);
        assert_eq!(s.net_rx_bps, 0);
    }

    #[tokio::test]
    async fn persist_once_writes_server_and_project_scopes_and_prunes() {
        let store = Store::open_in_memory().unwrap();
        let state = RwLock::new(State::new(10));
        // nothing collected yet: a no-op
        persist_once(&state, &store, 300).await;
        assert!(store.samples("server", 0).unwrap().is_empty());

        {
            let mut st = state.write().await;
            let now = 1_000_000u64;
            st.push(crate::metrics::Snapshot { ts: now, cpu_pct: 8.0, mem_used: 640, ..Default::default() });
            let mut live = ProjectLive::default();
            live.history.push_back(ProjectSample { ts: now, cpu_pct: 0.5, mem_bytes: 42, disk_bps: 7 });
            st.projects_live.insert("codo".into(), live);
        }
        // a stale point that must be pruned (older than 7 days vs now)
        store
            .insert_sample("server", &StoredSample { ts: 1_000_000 - KEEP_SECS - 5, cpu_pct: 1.0, mem_bytes: 1, disk_bps: 0, net_rx_bps: 0, net_tx_bps: 0 })
            .unwrap();

        persist_once(&state, &store, 300).await;
        let server = store.samples("server", 0).unwrap();
        assert_eq!(server.len(), 1, "stale point pruned, fresh one written");
        assert_eq!(server[0].ts, 1_000_000);
        assert!((server[0].cpu_pct - 8.0).abs() < 1e-9);
        let proj = store.samples("project:codo", 0).unwrap();
        assert_eq!(proj.len(), 1);
        assert_eq!(proj[0].disk_bps, 7);
    }
}
