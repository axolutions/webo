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

fn request(token: &str, method: &str, url: &str, body: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let req = ureq::request(method, url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "webo")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(20));
    let res = match body {
        Some(b) => req.send_json(b.clone()),
        None => req.call(),
    }
    .ok()?;
    // 204 (secret PUT) has no body
    Some(res.into_json::<serde_json::Value>().unwrap_or(serde_json::Value::Null))
}

fn get(token: &str, url: &str) -> Option<serde_json::Value> {
    request(token, "GET", url, None)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RepoInfo {
    pub owner: String,
    pub name: String,
    pub private: bool,
    pub language: Option<String>,
    pub pushed_at: i64,
    pub default_branch: String,
}

/// Parse the /user/repos listing.
pub fn parse_repo_list(json: &serde_json::Value) -> Vec<RepoInfo> {
    json.as_array()
        .map(|repos| {
            repos
                .iter()
                .filter_map(|r| {
                    Some(RepoInfo {
                        owner: r.get("owner")?.get("login")?.as_str()?.to_string(),
                        name: r.get("name")?.as_str()?.to_string(),
                        private: r.get("private").and_then(|v| v.as_bool()).unwrap_or(false),
                        language: r.get("language").and_then(|v| v.as_str()).map(String::from),
                        pushed_at: r
                            .get("pushed_at")
                            .and_then(|v| v.as_str())
                            .map(ts_of)
                            .unwrap_or(0),
                        default_branch: r
                            .get("default_branch")
                            .and_then(|v| v.as_str())
                            .unwrap_or("main")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn list_repos(token: &str) -> Vec<RepoInfo> {
    let base = api_base();
    get(
        token,
        &format!("{base}/user/repos?per_page=100&sort=pushed&affiliation=owner,organization_member"),
    )
    .as_ref()
    .map(parse_repo_list)
    .unwrap_or_default()
}

/// Fetch one file's text content (contents API, base64-encoded body).
pub fn get_file(token: &str, owner: &str, repo: &str, path: &str) -> Option<String> {
    use base64::Engine;
    let base = api_base();
    let json = get(token, &format!("{base}/repos/{owner}/{repo}/contents/{path}"))?;
    let content = json.get("content")?.as_str()?.replace(['\n', '\r'], "");
    let bytes = base64::engine::general_purpose::STANDARD.decode(content).ok()?;
    String::from_utf8(bytes).ok()
}

pub fn repo_info(token: &str, owner: &str, repo: &str) -> Option<RepoInfo> {
    let base = api_base();
    let r = get(token, &format!("{base}/repos/{owner}/{repo}"))?;
    Some(RepoInfo {
        owner: owner.to_string(),
        name: repo.to_string(),
        private: r.get("private").and_then(|v| v.as_bool()).unwrap_or(false),
        language: r.get("language").and_then(|v| v.as_str()).map(String::from),
        pushed_at: 0,
        default_branch: r
            .get("default_branch")
            .and_then(|v| v.as_str())
            .unwrap_or("main")
            .to_string(),
    })
}

/// One commit with all scaffold files, via the Git Data API:
/// ref → base commit → new tree → new commit → move the ref.
pub fn commit_files(
    token: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    files: &[crate::scaffold::PlanFile],
    message: &str,
) -> Result<String, String> {
    let base = api_base();
    let head = get(token, &format!("{base}/repos/{owner}/{repo}/git/ref/heads/{branch}"))
        .ok_or("could not read the branch head")?;
    let head_sha = head
        .pointer("/object/sha")
        .and_then(|v| v.as_str())
        .ok_or("branch head without sha")?
        .to_string();
    let commit = get(token, &format!("{base}/repos/{owner}/{repo}/git/commits/{head_sha}"))
        .ok_or("could not read the head commit")?;
    let base_tree = commit
        .pointer("/tree/sha")
        .and_then(|v| v.as_str())
        .ok_or("head commit without tree")?
        .to_string();

    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|f| {
            serde_json::json!({ "path": f.path, "mode": "100644", "type": "blob", "content": f.content })
        })
        .collect();
    let tree = request(
        token,
        "POST",
        &format!("{base}/repos/{owner}/{repo}/git/trees"),
        Some(&serde_json::json!({ "base_tree": base_tree, "tree": entries })),
    )
    .ok_or("could not create the tree")?;
    let tree_sha = tree.get("sha").and_then(|v| v.as_str()).ok_or("tree without sha")?;

    let new_commit = request(
        token,
        "POST",
        &format!("{base}/repos/{owner}/{repo}/git/commits"),
        Some(&serde_json::json!({ "message": message, "tree": tree_sha, "parents": [head_sha] })),
    )
    .ok_or("could not create the commit")?;
    let commit_sha = new_commit
        .get("sha")
        .and_then(|v| v.as_str())
        .ok_or("commit without sha")?
        .to_string();

    request(
        token,
        "PATCH",
        &format!("{base}/repos/{owner}/{repo}/git/refs/heads/{branch}"),
        Some(&serde_json::json!({ "sha": commit_sha })),
    )
    .ok_or("could not move the branch")?;
    Ok(commit_sha)
}

/// Encrypt a secret value for the repo's public key (libsodium sealed box).
pub fn seal_secret(public_key_b64: &str, value: &str) -> Option<String> {
    use base64::Engine;
    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64)
        .ok()?
        .try_into()
        .ok()?;
    let pk = crypto_box::PublicKey::from(key_bytes);
    let sealed = pk.seal(&mut crypto_box::aead::OsRng, value.as_bytes()).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(sealed))
}

pub fn set_secret(token: &str, owner: &str, repo: &str, name: &str, value: &str) -> Result<(), String> {
    let base = api_base();
    let key = get(token, &format!("{base}/repos/{owner}/{repo}/actions/secrets/public-key"))
        .ok_or("could not read the repo public key")?;
    let key_id = key.get("key_id").and_then(|v| v.as_str()).ok_or("public key without id")?;
    let key_b64 = key.get("key").and_then(|v| v.as_str()).ok_or("public key without key")?;
    let encrypted = seal_secret(key_b64, value).ok_or("could not encrypt the secret")?;
    request(
        token,
        "PUT",
        &format!("{base}/repos/{owner}/{repo}/actions/secrets/{name}"),
        Some(&serde_json::json!({ "encrypted_value": encrypted, "key_id": key_id })),
    )
    .ok_or("could not store the secret")?;
    Ok(())
}

/// Fast follow of a project's first deploy: refresh its builds every 10 s
/// (the regular sync is too slow for a live wizard) until the run completes.
pub async fn watch_first_deploy(store: Arc<Store>, slug: String, owner: String, name: String) {
    let Ok(token) = std::env::var("WEBO_GITHUB_TOKEN") else { return };
    let Ok(Some(p)) = store.project_by_slug(&slug) else { return };
    for _ in 0..90 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let base = api_base();
        let done = tokio::task::spawn_blocking({
            let (token, store, base) = (token.clone(), store.clone(), base.clone());
            let (owner, name) = (owner.clone(), name.clone());
            move || {
                let json = get(&token, &format!("{base}/repos/{owner}/{name}/actions/runs?per_page=5"))?;
                let builds = parse_runs(&json);
                let done = builds.first().is_some_and(|b| b.status == "completed");
                if !builds.is_empty() {
                    let _ = store.replace_builds(p.id, &builds);
                }
                Some(done)
            }
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
        if done {
            let _ = store.set_status(&slug, None);
            return;
        }
    }
    let _ = store.set_status(&slug, None);
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

    #[test]
    fn parse_repo_list_extracts_repos() {
        let payload = json!([
            {"name": "axofin", "owner": {"login": "murichristopher"}, "private": true,
             "language": "Ruby", "pushed_at": "2026-08-29T01:00:00Z", "default_branch": "main"},
            {"name": "broken"}
        ]);
        let repos = parse_repo_list(&payload);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].owner, "murichristopher");
        assert_eq!(repos[0].name, "axofin");
        assert!(repos[0].private);
        assert_eq!(repos[0].language.as_deref(), Some("Ruby"));
        assert_eq!(repos[0].default_branch, "main");
        assert!(repos[0].pushed_at > 0);
    }

    #[test]
    fn seal_secret_produces_a_sealed_box() {
        use base64::Engine;
        let sk = crypto_box::SecretKey::generate(&mut crypto_box::aead::OsRng);
        let pk_b64 = base64::engine::general_purpose::STANDARD.encode(sk.public_key().to_bytes());
        let sealed = seal_secret(&pk_b64, "super-secret").unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(sealed).unwrap();
        // ephemeral pk (32) + tag (16) + plaintext
        assert_eq!(bytes.len(), 32 + 16 + "super-secret".len());
        assert!(seal_secret("not base64!!", "x").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_once_fills_the_store_via_mock_api() {
        let _env = crate::testutil::env_lock();
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
