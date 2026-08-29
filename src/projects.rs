//! Project discovery and live metrics via the Docker socket (read-only).
//! A project is a docker compose project (label `com.docker.compose.project`);
//! containers without the label become single-container projects named after
//! themselves. The repo is inferred from a `ghcr.io/<owner>/<repo>` image and
//! the public domain from an optional `webo.domain` container label.

use crate::metrics::{ProjectContainer, ProjectLive, ProjectSample, State};
use crate::store::Store;
use bollard::container::{ListContainersOptions, StatsOptions};
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

pub const COMPOSE_LABEL: &str = "com.docker.compose.project";
pub const DOMAIN_LABEL: &str = "webo.domain";

/// `ghcr.io/owner/name[:tag|@digest]` → (owner, name). Anything else → None.
pub fn infer_repo(image: &str) -> Option<(String, String)> {
    let rest = image.strip_prefix("ghcr.io/")?;
    let rest = rest.split('@').next().unwrap_or(rest);
    let rest = rest.split(':').next().unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner, name))
}

/// Compose project name → URL-safe slug.
pub fn slug_for(compose_project: &str) -> String {
    compose_project
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// docker stats math: deltas of (container cpu ns, system cpu ns) → % of the
/// whole machine (like `docker stats`).
pub fn cpu_percent(cpu_delta: u64, system_delta: u64, online_cpus: u64) -> f32 {
    if system_delta == 0 {
        return 0.0;
    }
    (cpu_delta as f64 / system_delta as f64 * online_cpus as f64 * 100.0) as f32
}

/// RFC3339 (docker inspect StartedAt) → uptime in seconds against `now`.
pub fn uptime_from_rfc3339(started_at: &str, now: u64) -> u64 {
    time::OffsetDateTime::parse(started_at, &time::format_description::well_known::Rfc3339)
        .map(|t| now.saturating_sub(t.unix_timestamp().max(0) as u64))
        .unwrap_or(0)
}

fn now_ts() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

struct PrevStat {
    cpu_total: u64,
    system_total: u64,
    blkio_total: u64,
}

const HISTORY_CAP: usize = 5760; // 24 h at 15 s

fn push_history(live: &mut ProjectLive, sample: ProjectSample) {
    if live.history.len() == HISTORY_CAP {
        live.history.pop_front();
    }
    live.history.push_back(sample);
}

pub async fn run(state: Arc<RwLock<State>>, store: Arc<Store>, sample_secs: u64) {
    let Ok(docker) = Docker::connect_with_unix_defaults() else {
        // no docker socket: the Projects tab simply stays empty
        return;
    };
    let mut prev: HashMap<String, PrevStat> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(sample_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u64 = 0;
    let mut volume_sizes: HashMap<String, u64> = HashMap::new();

    loop {
        tick.tick().await;
        ticks += 1;
        let now = now_ts();

        let Ok(containers) = docker
            .list_containers(Some(ListContainersOptions::<String> { all: false, ..Default::default() }))
            .await
        else {
            continue;
        };

        // image sizes, keyed by BOTH tag and image id — a container whose tag
        // moved on (an old `latest`) only references the image by sha256 id
        let image_sizes: HashMap<String, u64> = docker
            .list_images::<String>(None)
            .await
            .map(|imgs| {
                imgs.into_iter()
                    .flat_map(|i| {
                        let size = i.size.max(0) as u64;
                        std::iter::once((i.id, size)).chain(i.repo_tags.into_iter().map(move |t| (t, size)))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // volume sizes are expensive (docker df): refresh every ~10 ticks
        if ticks % 10 == 1 {
            if let Ok(df) = docker.df().await {
                volume_sizes = df
                    .volumes
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| {
                        let size = v.usage_data.as_ref().map(|u| u.size.max(0) as u64)?;
                        Some((v.name, size))
                    })
                    .collect();
            }
        }

        let mut groups: HashMap<String, ProjectLive> = HashMap::new();
        let mut group_meta: HashMap<String, (String, Option<(String, String)>, Option<String>)> = HashMap::new();
        let mut seen_ids: Vec<String> = Vec::new();

        for c in &containers {
            let id = c.id.clone().unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            seen_ids.push(id.clone());
            let labels = c.labels.clone().unwrap_or_default();
            let name = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_else(|| id.chars().take(12).collect());
            let compose = labels.get(COMPOSE_LABEL).cloned().unwrap_or_else(|| name.clone());
            let slug = slug_for(&compose);
            let image = c.image.clone().unwrap_or_default();

            // stats (one-shot) + our own delta math
            let stats = docker
                .stats(&id, Some(StatsOptions { stream: false, one_shot: true }))
                .next()
                .await
                .and_then(|r| r.ok());
            let (mut cpu_pct, mut mem_bytes, mut disk_bps) = (0.0f32, 0u64, 0u64);
            if let Some(s) = stats {
                let cpu_total = s.cpu_stats.cpu_usage.total_usage;
                let system_total = s.cpu_stats.system_cpu_usage.unwrap_or(0);
                let online = s.cpu_stats.online_cpus.unwrap_or(1).max(1);
                let blkio_total: u64 = s
                    .blkio_stats
                    .io_service_bytes_recursive
                    .unwrap_or_default()
                    .iter()
                    .map(|e| e.value)
                    .sum();
                mem_bytes = s.memory_stats.usage.unwrap_or(0);
                if let Some(p) = prev.get(&id) {
                    cpu_pct = cpu_percent(
                        cpu_total.saturating_sub(p.cpu_total),
                        system_total.saturating_sub(p.system_total),
                        online,
                    );
                    disk_bps = blkio_total.saturating_sub(p.blkio_total) / sample_secs.max(1);
                }
                prev.insert(id.clone(), PrevStat { cpu_total, system_total, blkio_total });
            }

            // uptime via inspect (cheap for a handful of containers)
            let uptime_secs = match docker.inspect_container(&id, None).await {
                Ok(info) => info
                    .state
                    .and_then(|st| st.started_at)
                    .map(|t| uptime_from_rfc3339(&t, now))
                    .unwrap_or(0),
                Err(_) => 0,
            };

            let entry = groups.entry(slug.clone()).or_default();
            entry.cpu_pct += cpu_pct;
            entry.mem_bytes += mem_bytes;
            entry.disk_bps += disk_bps;
            entry.image_bytes += c
                .image_id
                .as_ref()
                .and_then(|id| image_sizes.get(id))
                .or_else(|| image_sizes.get(&image))
                .copied()
                .unwrap_or(0);
            entry.volume_bytes += c
                .mounts
                .as_ref()
                .map(|ms| {
                    ms.iter()
                        .filter_map(|m| m.name.as_ref())
                        .filter_map(|n| volume_sizes.get(n))
                        .sum::<u64>()
                })
                .unwrap_or(0);
            entry.containers.push(ProjectContainer {
                name,
                image: image.clone(),
                state: c.state.clone().unwrap_or_else(|| "unknown".into()),
                uptime_secs,
                cpu_pct,
                mem_bytes,
                disk_bps,
            });

            let meta = group_meta.entry(slug).or_insert((compose.clone(), None, None));
            if meta.1.is_none() {
                meta.1 = infer_repo(&image);
            }
            if meta.2.is_none() {
                meta.2 = labels.get(DOMAIN_LABEL).cloned();
            }
        }
        prev.retain(|k, _| seen_ids.contains(k));

        // persist discoveries (never clobbering user-made links)
        for (slug, (compose, repo, domain)) in &group_meta {
            let _ = store.upsert_discovered(
                slug,
                compose,
                repo.as_ref().map(|(o, n)| (o.as_str(), n.as_str())),
                domain.as_deref(),
                now as i64,
            );
        }

        // merge into shared state, carrying history forward
        let mut st = state.write().await;
        for (slug, mut live) in groups {
            let sample = ProjectSample { ts: now, cpu_pct: live.cpu_pct, mem_bytes: live.mem_bytes };
            if let Some(old) = st.projects_live.remove(&slug) {
                live.history = old.history;
            }
            push_history(&mut live, sample);
            st.projects_live.insert(slug, live);
        }
        st.projects_live.retain(|slug, _| group_meta.contains_key(slug));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_repo_only_from_ghcr_images() {
        assert_eq!(
            infer_repo("ghcr.io/murichristopher/codo:latest"),
            Some(("murichristopher".into(), "codo".into()))
        );
        assert_eq!(
            infer_repo("ghcr.io/axolutions/webo@sha256:abc"),
            Some(("axolutions".into(), "webo".into()))
        );
        assert_eq!(infer_repo("cloudflare/cloudflared:latest"), None);
        assert_eq!(infer_repo("postgres:16"), None);
        assert_eq!(infer_repo("ghcr.io/"), None);
    }

    #[test]
    fn slug_sanitizes_compose_names() {
        assert_eq!(slug_for("Codo"), "codo");
        assert_eq!(slug_for("my app!"), "my-app-");
        assert_eq!(slug_for("deploy_webo"), "deploy_webo");
    }

    #[test]
    fn cpu_percent_matches_docker_stats_semantics() {
        // container used 5% of the total system time on a 12-cpu machine
        let pct = cpu_percent(50, 1000, 12);
        assert!((pct - 60.0).abs() < 1e-3);
        assert_eq!(cpu_percent(10, 0, 4), 0.0, "no system delta yet");
    }

    #[test]
    fn uptime_parses_docker_timestamps() {
        let now = 1_800_000_000u64;
        let started = time::OffsetDateTime::from_unix_timestamp(now as i64 - 3600).unwrap();
        let s = started.format(&time::format_description::well_known::Rfc3339).unwrap();
        assert_eq!(uptime_from_rfc3339(&s, now), 3600);
        assert_eq!(uptime_from_rfc3339("not a date", now), 0);
    }

    /// Runs wherever a docker daemon is reachable (CI runner, dev machines);
    /// silently skips elsewhere. Exercises the whole discovery loop against
    /// a real disposable container.
    #[tokio::test(flavor = "multi_thread")]
    async fn discovery_sees_a_labeled_container() {
        use std::process::Command;
        let name = format!("webo-test-{}", std::process::id());
        let ok = Command::new("docker")
            .args([
                "run", "-d", "--rm", "--name", &name,
                "--label", "com.docker.compose.project=webo-test-proj",
                "--label", "webo.domain=test.example.com",
                "alpine:3", "sleep", "60",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("docker unavailable — skipping live discovery test");
            return;
        }

        let state = Arc::new(RwLock::new(State::new(10)));
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let handle = tokio::spawn(run(state.clone(), store.clone(), 1));
        tokio::time::sleep(Duration::from_millis(4500)).await;
        handle.abort();
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();

        let st = state.read().await;
        let live = st.projects_live.get("webo-test-proj").expect("project discovered");
        assert_eq!(live.containers.len(), 1);
        assert_eq!(live.containers[0].name, name);
        assert!(!live.history.is_empty());

        let p = store.project_by_slug("webo-test-proj").unwrap().expect("persisted");
        assert_eq!(p.source, "discovered");
        assert_eq!(p.domain.as_deref(), Some("test.example.com"));
    }

    #[test]
    fn history_is_capped() {
        let mut live = ProjectLive::default();
        for i in 0..(HISTORY_CAP + 5) {
            push_history(&mut live, ProjectSample { ts: i as u64, cpu_pct: 0.0, mem_bytes: 0 });
        }
        assert_eq!(live.history.len(), HISTORY_CAP);
        assert_eq!(live.history.front().unwrap().ts, 5);
    }
}
