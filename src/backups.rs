//! Postgres backups: a daily `pg_dump | gzip` per project database, kept for
//! seven days, restorable from the panel. Dumps land in the shared
//! `webo-backups` volume — the helper containers write to it by name and
//! webo mounts it at /backups, so listing and downloading are plain file
//! reads, no docker roundtrip.

use crate::store::{Database, Store};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const KEEP_PER_PROJECT: usize = 7;
/// The named volume the compose file gives webo at /backups (the helper
/// containers in db.rs bind it by this name).
#[cfg(test)]
const VOLUME: &str = "webo-backups";

pub fn backups_root() -> String {
    std::env::var("WEBO_BACKUPS_DIR").unwrap_or_else(|_| "/backups".into())
}

/// `20260901-041500.sql.gz` — sortable, safe, self-describing.
pub fn backup_filename(ts: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}.sql.gz",
        t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute(), t.second()
    )
}

/// Only names we generated are ever served or restored.
pub fn valid_backup_name(name: &str) -> bool {
    name.len() == 22
        && name.ends_with(".sql.gz")
        && name.as_bytes()[8] == b'-'
        && name[..15].chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// A slug is a path segment here — refuse anything that could escape.
fn safe_slug(slug: &str) -> bool {
    !slug.is_empty() && slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Serialize, PartialEq)]
pub struct BackupFile {
    pub file: String,
    pub size_bytes: u64,
    pub created_at: i64,
}

/// Lists a project's dumps, newest first — straight from the mounted volume.
pub fn list(root: &Path, slug: &str) -> Vec<BackupFile> {
    if !safe_slug(slug) {
        return Vec::new();
    }
    let dir = root.join(slug);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<BackupFile> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !valid_backup_name(&name) {
                return None;
            }
            let meta = e.metadata().ok()?;
            let created = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(BackupFile { file: name, size_bytes: meta.len(), created_at: created })
        })
        .collect();
    out.sort_by(|a, b| b.file.cmp(&a.file));
    out
}

/// Full path of one dump, only when the name is ours and the file exists.
pub fn file_path(root: &Path, slug: &str, file: &str) -> Option<PathBuf> {
    if !safe_slug(slug) || !valid_backup_name(file) {
        return None;
    }
    let p = root.join(slug).join(file);
    p.is_file().then_some(p)
}

/// Keeps the newest `keep` dumps, deletes the rest (they sort by name).
pub fn prune(root: &Path, slug: &str, keep: usize) -> usize {
    let mut removed = 0;
    for old in list(root, slug).into_iter().skip(keep) {
        if std::fs::remove_file(root.join(slug).join(&old.file)).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Runs `pg_dump | gzip` in a helper container on the app network, writing
/// into the shared volume. Returns the created filename.
pub async fn dump(db: &Database, network: &str, slug: &str) -> Result<String, String> {
    let (Some(container), Some(name), Some(user), Some(pass)) =
        (db.container.clone(), db.db_name.clone(), db.username.clone(), db.password.clone())
    else {
        return Err("database is not configured".into());
    };
    if !safe_slug(slug) {
        return Err("invalid project".into());
    }
    let file = backup_filename(now_ts());
    let script = format!(
        "mkdir -p /backups/{slug} && PGPASSWORD='{pass}' pg_dump -h {container} -U {user} -d {name} \
         | gzip > /backups/{slug}/{file}.part \
         && mv /backups/{slug}/{file}.part /backups/{slug}/{file} && echo WEBO_BACKUP_OK"
    );
    let out = crate::db::run_pg_helper(network, &script, "dump").await?;
    if out.contains("WEBO_BACKUP_OK") {
        Ok(file)
    } else {
        Err(format!("pg_dump failed: {}", out.trim().chars().take(300).collect::<String>()))
    }
}

/// Restores one dump with `gunzip | psql`. The caller confirms; this only
/// executes.
pub async fn restore(db: &Database, network: &str, slug: &str, file: &str) -> Result<(), String> {
    let (Some(container), Some(name), Some(user), Some(pass)) =
        (db.container.clone(), db.db_name.clone(), db.username.clone(), db.password.clone())
    else {
        return Err("database is not configured".into());
    };
    if !safe_slug(slug) || !valid_backup_name(file) {
        return Err("invalid backup name".into());
    }
    let script = format!(
        "[ -f /backups/{slug}/{file} ] || {{ echo WEBO_NO_FILE; exit 1; }}; \
         gunzip -c /backups/{slug}/{file} | PGPASSWORD='{pass}' psql -h {container} -U {user} -d {name} -v ON_ERROR_STOP=0 -q \
         && echo WEBO_RESTORE_OK"
    );
    let out = crate::db::run_pg_helper(network, &script, "restore").await?;
    if out.contains("WEBO_RESTORE_OK") {
        Ok(())
    } else if out.contains("WEBO_NO_FILE") {
        Err("backup file not found".into())
    } else {
        Err(format!("restore failed: {}", out.trim().chars().take(300).collect::<String>()))
    }
}

/// The daily pass: any postgres project whose newest dump is older than a day
/// gets a fresh one, then old files beyond the last 7 are dropped.
pub async fn run(store: Arc<Store>, every_secs: u64) {
    let mut tick = tokio::time::interval(Duration::from_secs(every_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let root = PathBuf::from(backups_root());
        let Ok(projects) = store.projects() else { continue };
        for p in projects {
            let Some(db) = store.database(p.id).ok().flatten() else { continue };
            if db.kind != "postgres" {
                continue;
            }
            let newest = list(&root, &p.slug).first().map(|b| b.created_at).unwrap_or(0);
            if now_ts() - newest < 24 * 3600 {
                continue;
            }
            let _ = dump(&db, &crate::server::app_network(), &p.slug).await;
            prune(&root, &p.slug, KEEP_PER_PROJECT);
        }
    }
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
    fn filenames_are_sortable_and_validated() {
        let f = backup_filename(1_756_700_100); // 2026-09-01 ~04:15 UTC
        assert!(valid_backup_name(&f), "{f}");
        assert!(f.ends_with(".sql.gz"));
        assert!(backup_filename(2_000_000_000) > f, "later timestamps sort after");

        assert!(valid_backup_name("20260901-041500.sql.gz"));
        assert!(!valid_backup_name("../../etc/passwd"));
        assert!(!valid_backup_name("20260901-041500.sql"));
        assert!(!valid_backup_name("2026090a-041500.sql.gz"));
        assert!(!valid_backup_name(""));
    }

    #[test]
    fn listing_prune_and_path_are_confined_to_the_project_dir() {
        let root = std::env::temp_dir().join(format!("webo-bk-{}", std::process::id()));
        let dir = root.join("loja");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [
            ("20260830-040000.sql.gz", "old"),
            ("20260901-040000.sql.gz", "new"),
            ("notes.txt", "junk that must never show up"),
        ] {
            std::fs::write(dir.join(name), body).unwrap();
        }

        let files = list(&root, "loja");
        assert_eq!(files.len(), 2, "junk is filtered");
        assert_eq!(files[0].file, "20260901-040000.sql.gz", "newest first");
        assert_eq!(files[0].size_bytes, 3);
        assert!(files[0].created_at > 0);

        // traversal is refused at both layers
        assert!(list(&root, "../etc").is_empty());
        assert!(file_path(&root, "loja", "../../x").is_none());
        assert!(file_path(&root, "loja", "20260901-040000.sql.gz").is_some());
        assert!(file_path(&root, "loja", "20990101-000000.sql.gz").is_none(), "absent file");

        assert_eq!(prune(&root, "loja", 1), 1, "keeps the newest");
        assert_eq!(list(&root, "loja").len(), 1);
        assert_eq!(prune(&root, "loja", 1), 0, "nothing left to prune");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dump_and_restore_roundtrip_against_a_real_postgres() {
        let available = std::process::Command::new("docker")
            .args(["info"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !available {
            eprintln!("docker unavailable — skipping backup roundtrip");
            return;
        }
        let slug = format!("webobk{}", std::process::id());
        let net = format!("{slug}-net");
        let _ = std::process::Command::new("docker").args(["network", "create", &net]).output();
        let db = crate::db::create_postgres(&slug, &net, "17").await.expect("db");
        crate::db::pg_query(&db, &net, "CREATE TABLE t (id int); INSERT INTO t VALUES (42);", true)
            .await
            .expect("seed");

        let file = dump(&db, &net, &slug).await.expect("dump created");
        assert!(valid_backup_name(&file));

        // damage the data, then bring it back
        crate::db::pg_query(&db, &net, "DELETE FROM t;", true).await.expect("wipe");
        restore(&db, &net, &slug, &file).await.expect("restore");
        let out = crate::db::pg_query(&db, &net, "SELECT id FROM t;", false).await.expect("read");
        assert!(out.contains("42"), "restored row came back: {out}");

        // a missing file is a clean error
        assert!(restore(&db, &net, &slug, "20990101-000000.sql.gz").await.is_err());

        crate::db::drop_postgres(db.container.as_deref().unwrap(), db.volume.as_deref()).await.ok();
        // the dump lives in the shared volume — clean it through a helper
        let _ = std::process::Command::new("docker")
            .args(["run", "--rm", "-v", &format!("{VOLUME}:/backups"), "alpine:3", "rm", "-rf", &format!("/backups/{slug}")])
            .output();
        let _ = std::process::Command::new("docker").args(["network", "rm", &net]).output();
    }
}
