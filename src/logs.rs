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

/// Pairs each error line with the lines that belong to it, so the stored
/// occurrence carries the whole story — a real stack trace, and the rest of
/// the message when the error spans several lines.
///
/// Two things were wrong before. Only stack frames counted as continuation,
/// so a multi-line message split; and a line that had already been absorbed
/// still opened an issue of its own on the next turn of the loop. Together
/// they turned one Clerk error into three issues and one npm failure into
/// five — the open-error count measured how chatty the logger was, not how
/// many things were broken.
pub fn error_blocks(lines: &[crate::store::LogLine]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = &lines[i];
        if !crate::errors::looks_like_error(&l.line, &l.stream) {
            i += 1;
            continue;
        }
        let mut message = l.line.clone();
        let mut j = i + 1;
        while j < lines.len() && j - i <= 20 {
            let f = &lines[j];
            if f.container != l.container || !continues(l, f) {
                break;
            }
            message.push('\n');
            message.push_str(&f.line);
            j += 1;
        }
        out.push((i, message));
        // whatever was folded in does not get to be an issue of its own
        i = j.max(i + 1);
    }
    out
}

/// Does `f` belong to the error that started at `head`?
///
/// A stack frame always does. Otherwise the line has to be part of the same
/// burst — same stream, same second — and not look like the start of a new
/// record. That last condition is what keeps the next request from being
/// swallowed: anything carrying its own level or timestamp opens its own
/// story.
fn continues(head: &crate::store::LogLine, f: &crate::store::LogLine) -> bool {
    if crate::errors::is_stack_frame(&f.line) {
        return true;
    }
    // Prose only folds in when it came out in the same breath: same stream,
    // same second. A looser window swallowed the next ordinary line, which is
    // the opposite mistake and a worse one — an error that eats the output
    // after it hides what really happened.
    if f.stream != head.stream || f.ts != head.ts {
        return false;
    }
    // indented text, and loggers that prefix every line of one failure
    let indented = f.line.starts_with(' ') || f.line.starts_with('\t');
    let same_label = shared_label(&head.line).is_some_and(|p| f.line.starts_with(p));
    indented || same_label || !crate::errors::starts_record(&f.line)
}

/// `npm error path /app` → `npm error `: the tag a logger repeats on every
/// line of the same failure.
fn shared_label(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    for tag in ["npm error ", "npm err! ", "yarn error ", "pnpm error "] {
        if lower.starts_with(tag) {
            return Some(&line[..tag.len()]);
        }
    }
    None
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

    /// The three shapes that used to split one failure into several issues,
    /// taken from the logs of real projects on the server.
    #[test]
    fn a_multi_line_failure_is_one_issue() {
        let mk = |ts: i64, c: &str, line: &str| crate::store::LogLine {
            ts, container: c.into(), stream: "stderr".into(), line: line.into(),
        };

        // Clerk: the help prose at the end used to become its own issue
        let clerk = vec![
            mk(10, "investos", "⨯ Error: Clerk: auth() was called but Clerk can't detect usage of clerkMiddleware(). Please ensure the following:"),
            mk(10, "investos", "- Your middleware file exists at ./middleware.(ts|js)"),
            mk(10, "investos", "If you've verified your configuration and are still seeing this error, there may be a runtime issue."),
            mk(10, "investos", "    at async l (.next/server/app/api/carteira/route.js:1:7545)"),
        ];
        let blocks = error_blocks(&clerk);
        assert_eq!(blocks.len(), 1, "one error, one issue");
        assert!(blocks[0].1.contains("still seeing this error"), "the prose belongs to it");
        assert!(blocks[0].1.contains("carteira/route.js"), "and so does the frame that blames the file");

        // npm: every line carries the same tag, so every line became an issue
        let npm = vec![
            mk(20, "ferraro", "npm error path /app"),
            mk(20, "ferraro", "npm error command failed"),
            mk(20, "ferraro", "npm error signal SIGTERM"),
            mk(20, "ferraro", "npm error command sh -c next start"),
        ];
        assert_eq!(error_blocks(&npm).len(), 1, "one SIGTERM, not four");

        // Postgres: the detail line is part of the exception above it
        let pg = vec![
            mk(30, "db", "ERROR:  column \"coluna_inexistente\" does not exist"),
            mk(30, "db", "ERROR:  column \"coluna_inexistente\" does not exist at character 38"),
        ];
        assert_eq!(error_blocks(&pg).len(), 1);

        // what must NOT fold in: the next second, the next container, the
        // next stream — anything that is a story of its own
        let apart = vec![
            mk(40, "app", "Error: boom"),
            mk(41, "app", "Error: a different boom one second later"),
        ];
        assert_eq!(error_blocks(&apart).len(), 2, "a later burst is a different failure");
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

    /// The whole collection loop against a real container: lines land in the
    /// index, an error (with its stack) becomes an issue, and a second pass
    /// does not duplicate anything.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_collects_indexes_and_tracks_errors() {
        let available = std::process::Command::new("docker")
            .args(["info"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !available {
            eprintln!("docker unavailable — skipping live collection test");
            return;
        }
        let name = format!("webo-collect-{}", std::process::id());
        let ok = std::process::Command::new("docker")
            .args([
                "run", "-d", "--name", &name,
                "--label", "com.docker.compose.project=webo-collect-proj",
                "alpine:3", "sh", "-c",
                "echo boot ok; echo 'Error: exploded' >&2; echo '    at handler (app.js:1:2)' >&2; sleep 60",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "container started");

        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        store.upsert_discovered("webo-collect-proj", "webo-collect-proj", None, None, 1).unwrap();
        let id = store.project_by_slug("webo-collect-proj").unwrap().unwrap().id;
        let handle = tokio::spawn(run(store.clone(), 1));
        // Wait for the STACK FRAME, not just any line: a pass that lands
        // between the error and its stack indexes the error alone, and the
        // frame that follows is not an error on its own — so the occurrence
        // would carry no trace. Waiting for the whole block makes the test
        // deterministic instead of racing the collector.
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let seen = store.search_logs(id, None, None, None, 50).unwrap();
            if seen.iter().any(|l| l.line.contains("at handler")) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await; // one more pass: dedupe path
        handle.abort();
        let _ = std::process::Command::new("docker").args(["rm", "-f", &name]).output();

        let lines = store.search_logs(id, None, None, None, 50).unwrap();
        assert!(lines.iter().any(|l| l.line.contains("boot ok")), "stdout indexed: {lines:?}");
        let boots = lines.iter().filter(|l| l.line.contains("boot ok")).count();
        assert_eq!(boots, 1, "a re-read never stores the same line twice");
        let issues = store.issues(id, Some("open")).unwrap();
        assert_eq!(issues.len(), 1, "the error line became one issue: {issues:?}");
        // the frame is attached when the block was collected in one pass, which
        // the wait above ensures; a split pass is a known limitation, not a
        // silent wrong answer — the issue simply has no blamed file
        assert_eq!(issues[0].culprit.as_deref(), Some("app.js:1:2"), "stack frame blamed");
        let events = store.issue_events(issues[0].id, 5).unwrap();
        assert!(events[0].message.contains("at handler"), "stack travels with the occurrence");
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
