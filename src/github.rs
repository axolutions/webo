//! GitHub integration (optional): with WEBO_GITHUB_TOKEN set, webo reads
//! workflow runs (builds) and GHCR container versions for every project with
//! a linked repo, caching them in the store. Without the token the Projects
//! tab still works — it just shows no builds/versions.

use crate::store::{Build, Store, Version};
use std::sync::Arc;
use std::time::Duration;

fn ts_of(rfc3339: &str) -> i64 {
    time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
        .map(|t| t.unix_timestamp())
        .unwrap_or(0)
}

/// Parse the /actions/runs response into cached builds.
pub fn parse_runs(json: &serde_json::Value) -> Vec<Build> {
    json.get("workflow_runs")
        .and_then(|v| v.as_array())
        .map(|runs| {
            runs.iter()
                .filter_map(|r| {
                    let started = r.get("run_started_at")?.as_str()?;
                    let updated = r.get("updated_at").and_then(|v| v.as_str()).unwrap_or(started);
                    Some(Build {
                        run_id: r.get("id")?.as_i64()?,
                        workflow: r.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        status: r.get("status")?.as_str()?.to_string(),
                        conclusion: r.get("conclusion").and_then(|v| v.as_str()).map(String::from),
                        commit_sha: r
                            .get("head_sha")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .chars()
                            .take(7)
                            .collect(),
                        commit_msg: r
                            .get("display_title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        branch: r
                            .get("head_branch")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        duration_secs: (ts_of(updated) - ts_of(started)).max(0),
                        created_at: ts_of(started),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the GHCR package versions response. The short-sha tag names the
/// version; a version also tagged `latest` is the one on air.
pub fn parse_versions(json: &serde_json::Value) -> Vec<Version> {
    json.as_array()
        .map(|versions| {
            versions
                .iter()
                .filter_map(|v| {
                    let tags: Vec<&str> = v
                        .get("metadata")?
                        .get("container")?
                        .get("tags")?
                        .as_array()?
                        .iter()
                        .filter_map(|t| t.as_str())
                        .collect();
                    if tags.is_empty() {
                        return None;
                    }
                    let sha_tag = tags
                        .iter()
                        .find(|t| t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit()))
                        .map(|t| t.chars().take(7).collect::<String>());
                    let tag = sha_tag.unwrap_or_else(|| tags[0].to_string());
                    Some(Version {
                        tag,
                        current: tags.contains(&"latest"),
                        size_bytes: None,
                        created_at: v
                            .get("created_at")
                            .and_then(|c| c.as_str())
                            .map(ts_of)
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Repo languages payload → webo tech kind: the highest-byte language that
/// maps to something the panel can draw (markup like HTML/CSS is skipped).
pub fn tech_from_languages(json: &serde_json::Value) -> Option<String> {
    let obj = json.as_object()?;
    let mut langs: Vec<(&String, i64)> =
        obj.iter().filter_map(|(k, v)| v.as_i64().map(|n| (k, n))).collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1));
    langs.into_iter().find_map(|(name, _)| {
        Some(match name.as_str() {
            "Rust" => "rust",
            "Ruby" => "ruby",
            "JavaScript" | "TypeScript" => "node",
            "Python" => "python",
            "Go" => "go",
            "Java" | "Kotlin" => "java",
            "Elixir" => "elixir",
            "PHP" => "web",
            "C" | "C++" => "generic",
            _ => return None,
        }
        .to_string())
    })
}

/// Overridable in tests (WEBO_GITHUB_API_BASE) — production talks to github.com.
fn api_base() -> String {
    std::env::var("WEBO_GITHUB_API_BASE").unwrap_or_else(|_| "https://api.github.com".into())
}

fn get(token: &str, url: &str) -> Option<serde_json::Value> {
    ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "webo")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(15))
        .call()
        .ok()?
        .into_json()
        .ok()
}

fn sync_once(store: &Store, token: &str) {
    let Ok(projects) = store.projects() else { return };
    for p in projects {
        let (Some(owner), Some(name)) = (p.repo_owner.as_deref(), p.repo_name.as_deref()) else {
            continue;
        };
        let base = api_base();
        if p.tech.is_none() {
            if let Some(t) = get(token, &format!("{base}/repos/{owner}/{name}/languages"))
                .as_ref()
                .and_then(tech_from_languages)
            {
                let _ = store.set_tech_if_empty(&p.slug, &t);
            }
        }
        if let Some(json) = get(
            token,
            &format!("{base}/repos/{owner}/{name}/actions/runs?per_page=10"),
        ) {
            let builds = parse_runs(&json);
            if !builds.is_empty() {
                let _ = store.replace_builds(p.id, &builds);
            }
        }
        // packages live under /users/... or /orgs/... depending on the owner
        let versions_json = get(
            token,
            &format!("{base}/users/{owner}/packages/container/{name}/versions?per_page=20"),
        )
        .or_else(|| {
            get(
                token,
                &format!("{base}/orgs/{owner}/packages/container/{name}/versions?per_page=20"),
            )
        });
        if let Some(json) = versions_json {
            let versions = parse_versions(&json);
            if !versions.is_empty() {
                let _ = store.replace_versions(p.id, &versions);
            }
        }
    }
}

pub async fn run(store: Arc<Store>, refresh_secs: u64) {
    let Ok(token) = std::env::var("WEBO_GITHUB_TOKEN") else { return };
    if token.trim().is_empty() {
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(refresh_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let store = store.clone();
        let token = token.clone();
        let _ = tokio::task::spawn_blocking(move || sync_once(&store, &token)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_runs_extracts_the_essentials() {
        let payload = json!({
            "workflow_runs": [
                {
                    "id": 111,
                    "name": "Deploy",
                    "status": "completed",
                    "conclusion": "success",
                    "head_sha": "4f44710ffa01f096d4b6bdd9c1b9a38b031f8c6c",
                    "head_branch": "main",
                    "display_title": "feat: something nice",
                    "run_started_at": "2026-08-28T21:00:00Z",
                    "updated_at": "2026-08-28T21:05:25Z"
                },
                {
                    "id": 112,
                    "status": "completed",
                    "conclusion": "failure",
                    "head_sha": "b835d11aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "head_branch": "main",
                    "display_title": "ci: broken",
                    "run_started_at": "2026-08-28T19:00:00Z",
                    "updated_at": "2026-08-28T19:02:42Z"
                }
            ]
        });
        let builds = parse_runs(&payload);
        assert_eq!(builds.len(), 2);
        assert_eq!(builds[0].run_id, 111);
        assert_eq!(builds[0].commit_sha, "4f44710");
        assert_eq!(builds[0].duration_secs, 325);
        assert_eq!(builds[1].conclusion.as_deref(), Some("failure"));
    }

    #[test]
    fn parse_runs_tolerates_garbage() {
        assert!(parse_runs(&json!({})).is_empty());
        assert!(parse_runs(&json!({"workflow_runs": [{"id": "not a number"}]})).is_empty());
    }

    #[test]
    fn parse_versions_picks_sha_tag_and_current() {
        let payload = json!([
            {
                "id": 1,
                "created_at": "2026-08-28T21:05:00Z",
                "metadata": { "container": { "tags": ["latest", "4f44710ffa01f096d4b6bdd9c1b9a38b031f8c6c"] } }
            },
            {
                "id": 2,
                "created_at": "2026-08-28T19:00:00Z",
                "metadata": { "container": { "tags": ["b835d11aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"] } }
            },
            {
                "id": 3,
                "created_at": "2026-08-28T18:00:00Z",
                "metadata": { "container": { "tags": [] } }
            }
        ]);
        let versions = parse_versions(&payload);
        assert_eq!(versions.len(), 2, "untagged (dangling) versions are skipped");
        assert_eq!(versions[0].tag, "4f44710");
        assert!(versions[0].current);
        assert_eq!(versions[1].tag, "b835d11");
        assert!(!versions[1].current);
    }

    #[test]
    fn parse_versions_tolerates_garbage() {
        assert!(parse_versions(&json!({})).is_empty());
        assert!(parse_versions(&json!([{"id": 1}])).is_empty());
    }

    #[test]
    fn tech_from_languages_maps_and_skips_markup() {
        assert_eq!(tech_from_languages(&json!({"Rust": 90000, "Shell": 100})).as_deref(), Some("rust"));
        assert_eq!(tech_from_languages(&json!({"HTML": 90000, "Ruby": 100})).as_deref(), Some("ruby"));
        assert_eq!(tech_from_languages(&json!({"TypeScript": 5})).as_deref(), Some("node"));
        assert_eq!(tech_from_languages(&json!({"Brainfuck": 5})), None);
        assert_eq!(tech_from_languages(&json!([])), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_once_fills_the_store_via_mock_api() {
        use axum::routing::get as axget;
        let runs = json!({
            "workflow_runs": [{
                "id": 7, "name": "Deploy", "status": "completed", "conclusion": "success",
                "head_sha": "abc1234def0000000000000000000000000000000",
                "head_branch": "main", "display_title": "feat: mocked",
                "run_started_at": "2026-08-28T21:00:00Z", "updated_at": "2026-08-28T21:01:00Z"
            }]
        });
        let versions = json!([{
            "id": 1, "created_at": "2026-08-28T21:02:00Z",
            "metadata": { "container": { "tags": ["latest", "abc1234def0000000000000000000000000000dd"] } }
        }]);
        let app = axum::Router::new()
            .route("/repos/{o}/{r}/languages", axget(|| async { axum::Json(json!({"Rust": 12345})) }))
            .route("/repos/{o}/{r}/actions/runs", axget({
                let runs = runs.clone();
                move || { let runs = runs.clone(); async move { axum::Json(runs) } }
            }))
            .route("/users/{o}/packages/container/{r}/versions", axget({
                let versions = versions.clone();
                move || { let versions = versions.clone(); async move { axum::Json(versions) } }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        std::env::set_var("WEBO_GITHUB_API_BASE", format!("http://{addr}"));
        let store = Store::open_in_memory().unwrap();
        store.upsert_discovered("webo", "webo", Some(("axolutions", "webo")), None, 1).unwrap();
        store.upsert_discovered("norepo", "norepo", None, None, 1).unwrap();
        tokio::task::block_in_place(|| sync_once(&store, "test-token"));
        std::env::remove_var("WEBO_GITHUB_API_BASE");

        let id = store.project_by_slug("webo").unwrap().unwrap().id;
        let builds = store.builds(id, 10).unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].workflow, "Deploy");
        assert_eq!(builds[0].commit_sha, "abc1234");
        let versions = store.versions(id, 10).unwrap();
        assert_eq!(versions.len(), 1);
        assert!(versions[0].current);
        assert_eq!(store.project_by_slug("webo").unwrap().unwrap().tech.as_deref(), Some("rust"));
    }
}
