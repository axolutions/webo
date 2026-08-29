//! SQLite-backed metadata store. Everything live (metrics, container state)
//! stays in memory; the store keeps only what must survive a restart:
//! project registrations/links and the cached GitHub builds/versions.
//! `server_id` exists from day one so multi-server can arrive without a
//! schema migration.

use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Project {
    pub id: i64,
    pub server_id: String,
    pub slug: String,
    pub name: String,
    pub source: String, // "discovered" | "registered"
    pub compose_project: Option<String>,
    pub repo_owner: Option<String>,
    pub repo_name: Option<String>,
    pub domain: Option<String>,
    pub tech: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Build {
    pub run_id: i64,
    pub workflow: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub commit_sha: String,
    pub commit_msg: String,
    pub branch: String,
    pub duration_secs: i64,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Version {
    pub tag: String,
    pub current: bool,
    pub size_bytes: Option<i64>,
    pub created_at: i64,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    server_id TEXT NOT NULL DEFAULT 'local',
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('discovered', 'registered')),
    compose_project TEXT,
    repo_owner TEXT,
    repo_name TEXT,
    domain TEXT,
    tech TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS builds (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id INTEGER NOT NULL,
    workflow TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    conclusion TEXT,
    commit_sha TEXT NOT NULL,
    commit_msg TEXT NOT NULL,
    branch TEXT NOT NULL,
    duration_secs INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (project_id, run_id)
);
CREATE INDEX IF NOT EXISTS idx_builds_project ON builds (project_id, created_at DESC);
CREATE TABLE IF NOT EXISTS versions (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    tag TEXT NOT NULL,
    current INTEGER NOT NULL DEFAULT 0,
    size_bytes INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (project_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_versions_project ON versions (project_id, created_at DESC);
";

/// Additive migrations for databases created by older versions —
/// CREATE TABLE IF NOT EXISTS never alters an existing table.
fn migrate(conn: &Connection) {
    let _ = conn.execute("ALTER TABLE builds ADD COLUMN workflow TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE projects ADD COLUMN tech TEXT", []);
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn);
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn);
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Discovery upsert: creates the project on first sight, and fills in
    /// repo/domain when discovery learns them — but NEVER overwrites values
    /// that already exist (a user-made link beats inference).
    pub fn upsert_discovered(
        &self,
        slug: &str,
        compose_project: &str,
        repo: Option<(&str, &str)>,
        domain: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO projects (slug, name, source, compose_project, repo_owner, repo_name, domain, created_at)
             VALUES (?1, ?1, 'discovered', ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (slug) DO UPDATE SET
                compose_project = excluded.compose_project,
                repo_owner = COALESCE(projects.repo_owner, excluded.repo_owner),
                repo_name = COALESCE(projects.repo_name, excluded.repo_name),
                domain = COALESCE(projects.domain, excluded.domain)",
            params![slug, compose_project, repo.map(|r| r.0), repo.map(|r| r.1), domain, now],
        )?;
        Ok(())
    }

    pub fn projects(&self) -> rusqlite::Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, server_id, slug, name, source, compose_project, repo_owner, repo_name, domain, tech, created_at
             FROM projects ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                server_id: r.get(1)?,
                slug: r.get(2)?,
                name: r.get(3)?,
                source: r.get(4)?,
                compose_project: r.get(5)?,
                repo_owner: r.get(6)?,
                repo_name: r.get(7)?,
                domain: r.get(8)?,
                tech: r.get(9)?,
                created_at: r.get(10)?,
            })
        })?;
        rows.collect()
    }

    /// Fills the detected technology once; a value already present wins
    /// (a template choice beats language inference).
    pub fn set_tech_if_empty(&self, slug: &str, tech: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE projects SET tech = ?1 WHERE slug = ?2 AND tech IS NULL",
            params![tech, slug],
        )?;
        Ok(())
    }

    pub fn project_by_slug(&self, slug: &str) -> rusqlite::Result<Option<Project>> {
        Ok(self.projects()?.into_iter().find(|p| p.slug == slug))
    }

    pub fn replace_builds(&self, project_id: i64, builds: &[Build]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for b in builds {
            tx.execute(
                "INSERT INTO builds (project_id, run_id, workflow, status, conclusion, commit_sha, commit_msg, branch, duration_secs, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (project_id, run_id) DO UPDATE SET
                    workflow = excluded.workflow,
                    status = excluded.status,
                    conclusion = excluded.conclusion,
                    duration_secs = excluded.duration_secs",
                params![
                    project_id, b.run_id, b.workflow, b.status, b.conclusion, b.commit_sha,
                    b.commit_msg, b.branch, b.duration_secs, b.created_at
                ],
            )?;
        }
        tx.commit()
    }

    pub fn builds(&self, project_id: i64, limit: usize) -> rusqlite::Result<Vec<Build>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, workflow, status, conclusion, commit_sha, commit_msg, branch, duration_secs, created_at
             FROM builds WHERE project_id = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project_id, limit as i64], |r| {
            Ok(Build {
                run_id: r.get(0)?,
                workflow: r.get(1)?,
                status: r.get(2)?,
                conclusion: r.get(3)?,
                commit_sha: r.get(4)?,
                commit_msg: r.get(5)?,
                branch: r.get(6)?,
                duration_secs: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        rows.collect()
    }

    pub fn replace_versions(&self, project_id: i64, versions: &[Version]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("UPDATE versions SET current = 0 WHERE project_id = ?1", params![project_id])?;
        for v in versions {
            tx.execute(
                "INSERT INTO versions (project_id, tag, current, size_bytes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (project_id, tag) DO UPDATE SET
                    current = excluded.current,
                    size_bytes = COALESCE(excluded.size_bytes, versions.size_bytes)",
                params![project_id, v.tag, v.current as i64, v.size_bytes, v.created_at],
            )?;
        }
        tx.commit()
    }

    pub fn versions(&self, project_id: i64, limit: usize) -> rusqlite::Result<Vec<Version>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tag, current, size_bytes, created_at
             FROM versions WHERE project_id = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![project_id, limit as i64], |r| {
            Ok(Version {
                tag: r.get(0)?,
                current: r.get::<_, i64>(1)? != 0,
                size_bytes: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn discovery_creates_and_updates_without_clobbering() {
        let s = store();
        s.upsert_discovered("codo", "codo", None, None, 100).unwrap();
        let p = s.project_by_slug("codo").unwrap().unwrap();
        assert_eq!(p.source, "discovered");
        assert_eq!(p.repo_owner, None);
        assert_eq!(p.server_id, "local");

        // discovery later learns the repo and domain
        s.upsert_discovered("codo", "codo", Some(("murichristopher", "codo")), Some("codo.example.com"), 200).unwrap();
        let p = s.project_by_slug("codo").unwrap().unwrap();
        assert_eq!(p.repo_owner.as_deref(), Some("murichristopher"));
        assert_eq!(p.domain.as_deref(), Some("codo.example.com"));
        assert_eq!(p.created_at, 100, "created_at keeps the first sighting");

        // inference can NOT overwrite an existing link
        s.upsert_discovered("codo", "codo", Some(("someone", "else")), Some("other.example.com"), 300).unwrap();
        let p = s.project_by_slug("codo").unwrap().unwrap();
        assert_eq!(p.repo_owner.as_deref(), Some("murichristopher"));
        assert_eq!(p.domain.as_deref(), Some("codo.example.com"));
    }

    #[test]
    fn projects_are_sorted_by_name() {
        let s = store();
        s.upsert_discovered("webo", "webo", None, None, 1).unwrap();
        s.upsert_discovered("codo", "codo", None, None, 2).unwrap();
        let names: Vec<String> = s.projects().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["codo", "webo"]);
    }

    #[test]
    fn builds_replace_updates_status_and_orders_desc() {
        let s = store();
        s.upsert_discovered("codo", "codo", None, None, 1).unwrap();
        let id = s.project_by_slug("codo").unwrap().unwrap().id;
        let b = |run_id, status: &str, created| Build {
            run_id,
            workflow: "Deploy".into(),
            status: status.into(),
            conclusion: None,
            commit_sha: "abc1234".into(),
            commit_msg: "msg".into(),
            branch: "main".into(),
            duration_secs: 60,
            created_at: created,
        };
        s.replace_builds(id, &[b(1, "in_progress", 10), b(2, "completed", 20)]).unwrap();
        s.replace_builds(id, &[b(1, "completed", 10)]).unwrap();
        let builds = s.builds(id, 10).unwrap();
        assert_eq!(builds.len(), 2);
        assert_eq!(builds[0].run_id, 2, "newest first");
        assert_eq!(builds[1].status, "completed", "run 1 was updated in place");
    }

    #[test]
    fn versions_replace_moves_the_current_flag() {
        let s = store();
        s.upsert_discovered("codo", "codo", None, None, 1).unwrap();
        let id = s.project_by_slug("codo").unwrap().unwrap().id;
        let v = |tag: &str, current, created| Version {
            tag: tag.into(),
            current,
            size_bytes: Some(96_000_000),
            created_at: created,
        };
        s.replace_versions(id, &[v("aaa", true, 10), v("bbb", false, 5)]).unwrap();
        s.replace_versions(id, &[v("ccc", true, 20), v("aaa", false, 10)]).unwrap();
        let versions = s.versions(id, 10).unwrap();
        assert_eq!(versions[0].tag, "ccc");
        assert!(versions[0].current);
        assert!(!versions.iter().any(|x| x.tag == "aaa" && x.current), "current moved off aaa");
    }

    #[test]
    fn builds_and_versions_isolated_per_project() {
        let s = store();
        s.upsert_discovered("a", "a", None, None, 1).unwrap();
        s.upsert_discovered("b", "b", None, None, 1).unwrap();
        let a = s.project_by_slug("a").unwrap().unwrap().id;
        let b_id = s.project_by_slug("b").unwrap().unwrap().id;
        s.replace_builds(a, &[Build {
            run_id: 1, workflow: "Deploy".into(), status: "completed".into(), conclusion: Some("success".into()),
            commit_sha: "x".into(), commit_msg: "m".into(), branch: "main".into(),
            duration_secs: 1, created_at: 1,
        }]).unwrap();
        assert_eq!(s.builds(a, 10).unwrap().len(), 1);
        assert!(s.builds(b_id, 10).unwrap().is_empty());
    }

    #[test]
    fn set_tech_fills_once_and_never_overwrites() {
        let s = store();
        s.upsert_discovered("codo", "codo", None, None, 1).unwrap();
        assert_eq!(s.project_by_slug("codo").unwrap().unwrap().tech, None);
        s.set_tech_if_empty("codo", "rust").unwrap();
        s.set_tech_if_empty("codo", "ruby").unwrap();
        assert_eq!(s.project_by_slug("codo").unwrap().unwrap().tech.as_deref(), Some("rust"));
    }

    #[test]
    fn open_migrates_a_pre_workflow_database() {
        let dir = std::env::temp_dir().join(format!("webo-migrate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        {
            // simulate a database created before the workflow column existed
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE projects (id INTEGER PRIMARY KEY, server_id TEXT NOT NULL DEFAULT 'local',
                    slug TEXT NOT NULL UNIQUE, name TEXT NOT NULL, source TEXT NOT NULL,
                    compose_project TEXT, repo_owner TEXT, repo_name TEXT, domain TEXT, created_at INTEGER NOT NULL);
                 CREATE TABLE builds (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL,
                    run_id INTEGER NOT NULL, status TEXT NOT NULL, conclusion TEXT,
                    commit_sha TEXT NOT NULL, commit_msg TEXT NOT NULL, branch TEXT NOT NULL,
                    duration_secs INTEGER NOT NULL, created_at INTEGER NOT NULL,
                    UNIQUE (project_id, run_id));",
            ).unwrap();
        }
        let s = Store::open(&path).unwrap();
        s.upsert_discovered("codo", "codo", None, None, 1).unwrap();
        let id = s.project_by_slug("codo").unwrap().unwrap().id;
        s.replace_builds(id, &[Build {
            run_id: 1, workflow: "Deploy".into(), status: "completed".into(),
            conclusion: Some("success".into()), commit_sha: "x".into(), commit_msg: "m".into(),
            branch: "main".into(), duration_secs: 1, created_at: 1,
        }]).unwrap();
        assert_eq!(s.builds(id, 10).unwrap()[0].workflow, "Deploy");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_on_disk_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("webo-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("webo.db");
        {
            let s = Store::open(&path).unwrap();
            s.upsert_discovered("codo", "codo", None, None, 1).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert!(s.project_by_slug("codo").unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
