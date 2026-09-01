//! Log collection: docker is the source, SQLite (FTS5) is the memory.
//! Reading straight from a container is fine for "what is happening now",
//! but a deploy recreates it and the history is gone — so every line is also
//! indexed, capped per project so a chatty app cannot fill the disk.

use crate::store::{LogLine, Store};
use bollard::container::{ListContainersOptions, LogsOptions};
use bollard::Docker;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;

/// 500 MB of lines per project, as agreed.
pub const MAX_BYTES_PER_PROJECT: i64 = 500 * 1024 * 1024;

/// Docker prefixes each line with an RFC3339 timestamp when asked to.
/// Returns (unix seconds, text) — the text keeps whatever the app wrote.
pub fn parse_line(raw: &str) -> Option<(i64, String)> {
    let raw = raw.trim_end_matches(['\n', '\r']);
    if raw.is_empty() {
        return None;
    }
    let (stamp, rest) = raw.split_once(' ')?;
    let ts = time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
        .ok()?
        .unix_timestamp();
    Some((ts, rest.to_string()))
}

/// Pairs each error line with the stack frames that follow it, so the stored
/// occurrence carries the whole story — the panel shows a real stack trace,
/// not a lonely first line.
pub fn error_blocks(lines: &[crate::store::LogLine]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !crate::errors::looks_like_error(&l.line, &l.stream) {
            continue;
        }
        let mut message = l.line.clone();
        for follow in lines.iter().skip(i + 1).take(12) {
            if follow.container == l.container && crate::errors::is_stack_frame(&follow.line) {
                message.push('\n');
                message.push_str(&follow.line);
            } else {
                break;
            }
        }
        out.push((i, message));
    }
    out
}

/// Lines newer than `since`, so a re-read never stores the same line twice.
pub fn newer_than(lines: Vec<(i64, String, String)>, since: Option<i64>) -> Vec<(i64, String, String)> {
    match since {
        Some(s) => lines.into_iter().filter(|(ts, _, _)| *ts > s).collect(),
        None => lines,
    }
}

async fn fetch(docker: &Docker, id: &str, since: Option<i64>) -> Vec<(i64, String, String)> {
    let mut stream = docker.logs(
        id,
        Some(LogsOptions::<String> {
            stdout: true,
            stderr: true,
            timestamps: true,
            since: since.unwrap_or(0),
            tail: if since.is_some() { "all".into() } else { "500".into() },
            ..Default::default()
        }),
    );
    let mut out = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        let stream_name = match chunk {
            bollard::container::LogOutput::StdErr { .. } => "stderr",
            _ => "stdout",
        };
        for raw in chunk.to_string().split('\n') {
            if let Some((ts, text)) = parse_line(raw) {
                out.push((ts, stream_name.to_string(), text));
            }
        }
    }
    out
}

/// Reads a container's tail without touching the index — the "now" view.
pub async fn tail(container: &str, lines: usize) -> Vec<LogLine> {
    let Ok(docker) = Docker::connect_with_unix_defaults() else { return Vec::new() };
    let mut stream = docker.logs(
        container,
        Some(LogsOptions::<String> {
            stdout: true,
            stderr: true,
            timestamps: true,
            tail: lines.to_string(),
            ..Default::default()
        }),
    );
    let mut out = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        let stream_name = match chunk {
            bollard::container::LogOutput::StdErr { .. } => "stderr",
            _ => "stdout",
        };
        for raw in chunk.to_string().split('\n') {
            if let Some((ts, text)) = parse_line(raw) {
                out.push(LogLine {
                    ts,
                    container: container.to_string(),
                    stream: stream_name.to_string(),
                    line: text,
                });
            }
        }
    }
    out
}

pub async fn run(store: Arc<Store>, every_secs: u64) {
    let Ok(docker) = Docker::connect_with_unix_defaults() else { return };
    let mut tick = tokio::time::interval(Duration::from_secs(every_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Ok(projects) = store.projects() else { continue };
        let Ok(containers) = docker
            .list_containers(Some(ListContainersOptions::<String> { all: false, ..Default::default() }))
            .await
        else {
            continue;
        };
        for p in projects {
            let compose = p.compose_project.clone().unwrap_or_else(|| p.slug.clone());
            for c in &containers {
                let belongs = c
                    .labels
                    .as_ref()
                    .and_then(|l| l.get(crate::projects::COMPOSE_LABEL))
                    .is_some_and(|v| v == &compose);
                if !belongs {
                    continue;
                }
                let Some(id) = c.id.clone() else { continue };
                let name = c
                    .names
                    .as_ref()
                    .and_then(|n| n.first())
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| id.chars().take(12).collect());
                let since = store.last_log_ts(p.id, &name).ok().flatten();
                let fetched = fetch(&docker, &id, since).await;
                let fresh = newer_than(fetched, since);
                if fresh.is_empty() {
                    continue;
                }
                let lines: Vec<LogLine> = fresh
                    .into_iter()
                    .map(|(ts, stream, line)| LogLine { ts, container: name.clone(), stream, line })
                    .collect();
                let _ = store.insert_logs(p.id, &lines);
                // the same lines feed error tracking, so every app gets it
                // without installing anything; the stack frames that follow an
                // error travel with it as one occurrence
                for (i, message) in error_blocks(&lines) {
                    let l = &lines[i];
                    // fingerprint the cleaned title, not the raw line: the
                    // same bug logged by the app and by the framework must
                    // land on one issue
                    let title = crate::errors::title_of(&l.line);
                    let _ = store.record_error(
                        p.id,
                        &crate::errors::fingerprint(&title),
                        &title,
                        "server",
                        &l.container,
                        &message,
                        l.ts,
                        crate::errors::culprit_of(&message).as_deref(),
                    );
                }
            }
            let _ = store.prune_logs(p.id, MAX_BYTES_PER_PROJECT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_timestamps_are_split_from_the_text() {
        let (ts, text) = parse_line("2026-09-01T03:20:15.123456789Z server started on :3000\n").unwrap();
        assert!(ts > 1_700_000_000);
        assert_eq!(text, "server started on :3000");
        // a line the app wrote with its own spacing keeps it
        let (_, text) = parse_line("2026-09-01T03:20:15Z   GET /health 200").unwrap();
        assert_eq!(text, "  GET /health 200");
        assert!(parse_line("").is_none());
        assert!(parse_line("sem timestamp aqui").is_none());
    }

    #[test]
    fn re_reading_never_stores_the_same_line_twice() {
        let lines = vec![
            (100, "stdout".to_string(), "old".to_string()),
            (200, "stdout".to_string(), "boundary".to_string()),
            (300, "stderr".to_string(), "new".to_string()),
        ];
        let fresh = newer_than(lines.clone(), Some(200));
        assert_eq!(fresh.len(), 1, "the line at the boundary was already stored");
        assert_eq!(fresh[0].2, "new");
        assert_eq!(newer_than(lines, None).len(), 3, "a first read takes everything");
    }

    #[test]
    fn stacks_travel_with_their_error() {
        let mk = |ts: i64, c: &str, line: &str| crate::store::LogLine {
            ts, container: c.into(), stream: "stderr".into(), line: line.into(),
        };
        let lines = vec![
            mk(1, "app", "GET / 200"),
            mk(2, "app", "TypeError: Cannot read properties of null (reading 'valor')"),
            mk(2, "app", "    at w (.next/server/app/api/quebra/route.js:1:823)"),
            mk(2, "app", "    at async POST (route.js:31:5)"),
            mk(3, "app", "next line of ordinary output"),
            mk(4, "db", "ERROR:  syntax error at or near \"1\""),
        ];
        let blocks = error_blocks(&lines);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, 1);
        assert!(blocks[0].1.contains("route.js:1:823"), "stack attached");
        assert!(!blocks[0].1.contains("ordinary output"), "capture stops at normal lines");
        assert_eq!(blocks[1].1.lines().count(), 1, "error without stack stays one line");
        assert_eq!(
            crate::errors::culprit_of(&blocks[0].1).as_deref(),
            Some(".next/server/app/api/quebra/route.js:1:823")
        );
        // a frame from ANOTHER container never glues onto this error
        let mixed = vec![
            mk(1, "app", "Error: boom"),
            mk(1, "db", "    at other (x.js:1:1)"),
        ];
        assert_eq!(error_blocks(&mixed)[0].1.lines().count(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tail_reads_a_real_container() {
        let available = std::process::Command::new("docker")
            .args(["info"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !available {
            eprintln!("docker unavailable — skipping tail test");
            return;
        }
        let name = format!("webo-logs-{}", std::process::id());
        let _ = std::process::Command::new("docker")
            .args(["run", "--name", &name, "alpine:3", "sh", "-c", "echo primeira linha; echo segunda linha >&2"])
            .output();
        let lines = tail(&name, 10).await;
        assert!(lines.iter().any(|l| l.line.contains("primeira linha")));
        assert!(
            lines.iter().any(|l| l.stream == "stderr" && l.line.contains("segunda")),
            "stderr is labelled: {lines:?}"
        );
        let _ = std::process::Command::new("docker").args(["rm", "-f", &name]).output();
    }
}
