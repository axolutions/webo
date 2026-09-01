//! Databases per project: one Postgres container each, or a SQLite file
//! discovered inside the project's volumes. Everything the panel does goes
//! through short-lived helper containers, so the app is never touched.

use crate::store::Database;
use bollard::container::{Config, CreateContainerOptions, LogsOptions, StartContainerOptions};
use bollard::Docker;
use futures_util::StreamExt;
use serde::Serialize;

pub const PG_IMAGE: &str = "postgres:17-alpine";
const SQLITE_IMAGE: &str = "alpine:3";
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// Postgres identifiers webo generates: lowercase, no dashes.
pub fn pg_ident(slug: &str) -> String {
    let ident: String = slug
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let ident = ident.trim_matches('_').to_string();
    let ident = if ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("db_{ident}")
    } else {
        ident
    };
    ident.chars().take(48).collect()
}

/// URL-safe password, no characters that would need escaping in a URL.
pub fn generate_password() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let state = RandomState::new();
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    (0..32)
        .map(|i| {
            let mut h = state.build_hasher();
            h.write_u64(seed.wrapping_add((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
            ALPHABET[(h.finish() % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

pub fn database_url(user: &str, password: &str, host: &str, db: &str) -> String {
    format!("postgres://{user}:{password}@{host}:5432/{db}")
}

/// True when the first bytes are SQLite's file header.
pub fn is_sqlite_header(bytes: &[u8]) -> bool {
    bytes.len() >= SQLITE_MAGIC.len() && &bytes[..SQLITE_MAGIC.len()] == SQLITE_MAGIC
}

/// Files worth probing for the SQLite header.
pub fn looks_like_db_file(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".db", ".sqlite", ".sqlite3", ".db3"].iter().any(|ext| lower.ends_with(ext))
        && !lower.ends_with("-wal")
        && !lower.ends_with("-shm")
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub row_count: usize,
    pub truncated: bool,
}

/// Splits psql/sqlite pipe-separated output into columns and rows.
pub fn parse_table_output(out: &str, limit: usize) -> QueryResult {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let columns: Vec<String> = lines
        .next()
        .map(|h| h.split('|').map(|c| c.trim().to_string()).collect())
        .unwrap_or_default();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    for line in lines {
        if rows.len() >= limit {
            truncated = true;
            break;
        }
        rows.push(line.split('|').map(|c| c.trim().to_string()).collect());
    }
    QueryResult { columns, row_count: rows.len(), rows, truncated }
}

/// Statements that change data or schema — the panel refuses them unless the
/// caller explicitly asked for write mode.
pub fn is_write_statement(sql: &str) -> bool {
    let mut cleaned = String::new();
    for line in sql.lines() {
        let line = line.split("--").next().unwrap_or("");
        cleaned.push_str(line);
        cleaned.push(' ');
    }
    let head = cleaned.trim_start().to_ascii_lowercase();
    const READ_ONLY: [&str; 4] = ["select", "with", "explain", "show"];
    !READ_ONLY.iter().any(|k| head.starts_with(k))
}

/// Runs a command in a throwaway container and returns its combined output.
async fn run_helper(docker: &Docker, config: Config<String>, name: &str) -> Result<String, String> {
    let opts = CreateContainerOptions { name: name.to_string(), platform: None };
    let created = docker.create_container(Some(opts), config).await.map_err(|e| e.to_string())?;
    let id = created.id;
    docker
        .start_container(&id, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| e.to_string())?;
    let mut wait = docker.wait_container::<String>(&id, None);
    let _ = wait.next().await;
    let mut logs = docker.logs::<String>(
        &id,
        Some(LogsOptions { stdout: true, stderr: true, ..Default::default() }),
    );
    let mut out = String::new();
    while let Some(Ok(chunk)) = logs.next().await {
        out.push_str(&chunk.to_string());
    }
    let _ = docker
        .remove_container(&id, Some(bollard::container::RemoveContainerOptions { force: true, ..Default::default() }))
        .await;
    Ok(out)
}

fn helper_name(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("webo-{prefix}-{n:x}")
}

/// Creates the project's Postgres container (idempotent) and waits for it.
pub async fn create_postgres(slug: &str, network: &str) -> Result<Database, String> {
    let docker = Docker::connect_with_unix_defaults().map_err(|e| e.to_string())?;
    let ident = pg_ident(slug);
    let container = format!("{slug}-db");
    let password = generate_password();
    let volume = format!("{slug}-db-data");

    let config = Config {
        image: Some(PG_IMAGE.to_string()),
        env: Some(vec![
            format!("POSTGRES_DB={ident}"),
            format!("POSTGRES_USER={ident}"),
            format!("POSTGRES_PASSWORD={password}"),
        ]),
        labels: Some(std::collections::HashMap::from([
            (crate::projects::COMPOSE_LABEL.to_string(), slug.to_string()),
            ("webo.role".to_string(), "database".to_string()),
        ])),
        host_config: Some(bollard::models::HostConfig {
            binds: Some(vec![format!("{volume}:/var/lib/postgresql/data")]),
            network_mode: Some(network.to_string()),
            restart_policy: Some(bollard::models::RestartPolicy {
                name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opts = CreateContainerOptions { name: container.clone(), platform: None };
    match docker.create_container(Some(opts), config).await {
        Ok(_) => {}
        Err(e) if e.to_string().contains("409") || e.to_string().contains("already in use") => {
            return Err("database container already exists".into())
        }
        Err(e) => return Err(e.to_string()),
    }
    docker
        .start_container(&container, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| e.to_string())?;

    // wait for it to accept connections
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let out = run_helper(
            &docker,
            Config {
                image: Some(PG_IMAGE.to_string()),
                cmd: Some(vec!["pg_isready".into(), "-h".into(), container.clone(), "-U".into(), ident.clone()]),
                host_config: Some(bollard::models::HostConfig {
                    network_mode: Some(network.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &helper_name("ready"),
        )
        .await
        .unwrap_or_default();
        if out.contains("accepting connections") {
            break;
        }
    }

    Ok(Database {
        kind: "postgres".into(),
        container: Some(container),
        db_name: Some(ident.clone()),
        username: Some(ident),
        password: Some(password),
        volume: Some(volume),
        file_path: None,
        persisted: true,
        created_at: now_ts(),
    })
}

pub async fn drop_postgres(container: &str, volume: Option<&str>) -> Result<(), String> {
    let docker = Docker::connect_with_unix_defaults().map_err(|e| e.to_string())?;
    let _ = docker
        .remove_container(
            container,
            Some(bollard::container::RemoveContainerOptions { force: true, ..Default::default() }),
        )
        .await;
    if let Some(v) = volume {
        let _ = docker.remove_volume(v, None).await;
    }
    Ok(())
}

/// Runs SQL against the project's Postgres through a helper container.
pub async fn pg_query(db: &Database, network: &str, sql: &str, write: bool) -> Result<String, String> {
    let (Some(container), Some(name), Some(user), Some(pass)) =
        (db.container.clone(), db.db_name.clone(), db.username.clone(), db.password.clone())
    else {
        return Err("database is not configured".into());
    };
    let docker = Docker::connect_with_unix_defaults().map_err(|e| e.to_string())?;
    let guarded = if write {
        sql.to_string()
    } else {
        format!("SET default_transaction_read_only = on;\n{sql}")
    };
    let script = format!(
        "PGPASSWORD='{pass}' psql -h {container} -U {user} -d {name} -A -F'|' -v ON_ERROR_STOP=1 <<'WEBOSQL'\n{guarded}\nWEBOSQL"
    );
    run_helper(
        &docker,
        Config {
            image: Some(PG_IMAGE.to_string()),
            cmd: Some(vec!["sh".into(), "-c".into(), script]),
            env: Some(vec!["PGCONNECT_TIMEOUT=10".into()]),
            host_config: Some(bollard::models::HostConfig {
                network_mode: Some(network.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
        &helper_name("psql"),
    )
    .await
}

/// Looks for a SQLite file in the volumes and binds a project's containers use.
pub async fn detect_sqlite(slug: &str) -> Option<Database> {
    let docker = Docker::connect_with_unix_defaults().ok()?;
    let mut filters = std::collections::HashMap::new();
    filters.insert(
        "label".to_string(),
        vec![format!("{}={}", crate::projects::COMPOSE_LABEL, slug)],
    );
    let containers = docker
        .list_containers(Some(bollard::container::ListContainersOptions { all: true, filters, ..Default::default() }))
        .await
        .ok()?;

    for c in containers {
        if c.labels.as_ref().and_then(|l| l.get("webo.role")).is_some_and(|r| r == "database") {
            continue; // our own Postgres
        }
        for m in c.mounts.unwrap_or_default() {
            let Some(dest) = m.destination.clone() else { continue };
            let source = m.name.clone().or_else(|| m.source.clone());
            let Some(source) = source else { continue };
            let bind = match m.typ {
                Some(bollard::models::MountPointTypeEnum::BIND) => format!("{source}:/probe:ro"),
                _ => format!("{source}:/probe:ro"),
            };
            let listing = run_helper(
                &docker,
                Config {
                    image: Some(SQLITE_IMAGE.to_string()),
                    cmd: Some(vec![
                        "sh".into(),
                        "-c".into(),
                        // print files whose first bytes are the SQLite header
                        "find /probe -maxdepth 4 -type f -size +0 2>/dev/null | while read f; do \
                         head -c 16 \"$f\" 2>/dev/null | grep -qa 'SQLite format 3' && echo \"$f\"; done | head -3".into(),
                    ]),
                    host_config: Some(bollard::models::HostConfig {
                        binds: Some(vec![bind]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                &helper_name("probe"),
            )
            .await
            .unwrap_or_default();
            if let Some(found) = listing.lines().map(|l| l.trim()).find(|l| l.starts_with("/probe/")) {
                let rel = found.trim_start_matches("/probe");
                return Some(Database {
                    kind: "sqlite".into(),
                    container: None,
                    db_name: None,
                    username: None,
                    password: None,
                    volume: Some(source),
                    file_path: Some(format!("{}{}", dest.trim_end_matches('/'), rel)),
                    persisted: true,
                    created_at: now_ts(),
                });
            }
        }
    }
    None
}

/// Runs SQL against a SQLite file by mounting the same volume in a helper —
/// read-only unless write mode was asked for, so the app keeps its locks.
pub async fn sqlite_query(db: &Database, sql: &str, write: bool) -> Result<String, String> {
    let (Some(volume), Some(file)) = (db.volume.clone(), db.file_path.clone()) else {
        return Err("sqlite file not located".into());
    };
    let docker = Docker::connect_with_unix_defaults().map_err(|e| e.to_string())?;
    let name = file.rsplit('/').next().unwrap_or("app.db").to_string();
    let bind = if write { format!("{volume}:/probe") } else { format!("{volume}:/probe:ro") };
    let script = format!(
        "apk add --no-cache sqlite >/dev/null 2>&1; f=$(find /probe -maxdepth 4 -name '{name}' | head -1); \
         [ -z \"$f\" ] && {{ echo 'file not found'; exit 1; }}; \
         sqlite3 -header -separator '|' \"$f\" <<'WEBOSQL'\n{sql}\nWEBOSQL"
    );
    run_helper(
        &docker,
        Config {
            image: Some(SQLITE_IMAGE.to_string()),
            cmd: Some(vec!["sh".into(), "-c".into(), script]),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(vec![bind]),
                ..Default::default()
            }),
            ..Default::default()
        },
        &helper_name("sqlite"),
    )
    .await
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_ident_is_always_a_valid_identifier() {
        assert_eq!(pg_ident("ferraro-producoes"), "ferraro_producoes");
        assert_eq!(pg_ident("Loja.Nova"), "loja_nova");
        assert_eq!(pg_ident("2fast"), "db_2fast");
        assert_eq!(pg_ident("--x--"), "x");
        assert!(pg_ident(&"a".repeat(80)).len() <= 48);
    }

    #[test]
    fn passwords_are_url_safe_and_unique() {
        let a = generate_password();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "no escaping needed in a URL");
        let many: std::collections::HashSet<String> = (0..8).map(|_| generate_password()).collect();
        assert!(many.len() > 1, "passwords must differ");
        let url = database_url("loja", &a, "loja-db", "loja");
        assert!(url.starts_with("postgres://loja:"));
        assert!(url.ends_with("@loja-db:5432/loja"));
    }

    #[test]
    fn sqlite_header_and_file_names() {
        assert!(is_sqlite_header(b"SQLite format 3\0rest of the file"));
        assert!(!is_sqlite_header(b"not a database"));
        assert!(!is_sqlite_header(b"SQLite"));
        assert!(looks_like_db_file("/data/app.sqlite3"));
        assert!(looks_like_db_file("dev.db"));
        assert!(!looks_like_db_file("/data/app.db-wal"));
        assert!(!looks_like_db_file("/data/notes.txt"));
    }

    #[test]
    fn write_statements_are_recognised() {
        assert!(!is_write_statement("SELECT * FROM users"));
        assert!(!is_write_statement("  with x as (select 1) select * from x"));
        assert!(!is_write_statement("EXPLAIN ANALYZE SELECT 1"));
        assert!(is_write_statement("DELETE FROM users"));
        assert!(is_write_statement("drop table users"));
        assert!(is_write_statement("INSERT INTO t VALUES (1)"));
        // a comment must not disguise a write
        assert!(is_write_statement("-- select\nDROP TABLE users"));
    }

    #[test]
    fn table_output_is_parsed_into_columns_and_rows() {
        let out = "id|email\n1|a@b.com\n2|c@d.com\n";
        let r = parse_table_output(out, 100);
        assert_eq!(r.columns, vec!["id", "email"]);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[1][1], "c@d.com");
        assert!(!r.truncated);

        let r = parse_table_output(out, 1);
        assert_eq!(r.rows.len(), 1);
        assert!(r.truncated, "hitting the limit is reported");

        let empty = parse_table_output("", 10);
        assert!(empty.columns.is_empty() && empty.rows.is_empty());
    }
}
