//! Templates and the scaffold plan: which files webo will commit into a repo
//! to make it deployable. Pure logic — the GitHub side lives in `github.rs`.

use serde::Serialize;

pub const SECRET_NAMES: [&str; 3] = ["WEBO_DEPLOY_TOKEN", "TS_OAUTH_CLIENT_ID", "TS_OAUTH_SECRET"];

#[derive(Clone, Copy, PartialEq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Template {
    Rails,
    Next,
}

/// Detect the template from repo files: a Gemfile that mentions rails, or a
/// package.json with next in the dependencies.
pub fn detect(gemfile: Option<&str>, package_json: Option<&str>) -> Option<Template> {
    if gemfile.is_some_and(|g| g.contains("rails")) {
        return Some(Template::Rails);
    }
    if let Some(pkg) = package_json {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(pkg) {
            for key in ["dependencies", "devDependencies"] {
                if json.get(key).and_then(|d| d.get("next")).is_some() {
                    return Some(Template::Next);
                }
            }
        }
    }
    None
}

/// The Ruby the app asks for, so the image matches it — a Gemfile pinned to
/// 3.2 built on a 3.3 image fails with "your Gemfile specified", and the
/// user has no way to see why. `.ruby-version` wins; the Gemfile's `ruby`
/// line is the fallback. Only major.minor is used: patch releases move.
pub fn ruby_version(ruby_version_file: Option<&str>, gemfile: Option<&str>) -> String {
    let clean = |raw: &str| -> Option<String> {
        let v: String = raw
            .trim()
            .trim_start_matches("ruby-")
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts = v.split('.');
        let major = parts.next().filter(|p| !p.is_empty())?;
        let minor = parts.next().filter(|p| !p.is_empty())?;
        // anything older than 3.0 is not worth an image that exists
        (major.parse::<u32>().ok()? >= 3).then(|| format!("{major}.{minor}"))
    };
    ruby_version_file
        .and_then(|f| f.lines().next())
        .and_then(clean)
        .or_else(|| {
            gemfile?.lines().find_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix("ruby ")?;
                let quoted = rest.trim().trim_start_matches(['"', '\'']);
                clean(quoted)
            })
        })
        .unwrap_or_else(|| DEFAULT_RUBY.to_string())
}

/// What the template builds on when the repo says nothing.
pub const DEFAULT_RUBY: &str = "3.3";

#[derive(Clone, Debug, Serialize)]
pub struct PlanFile {
    pub path: String,
    pub content: String,
}

/// The files webo will commit. An existing Dockerfile is respected — the
/// owner's build beats the template.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    template: Template,
    slug: &str,
    owner: &str,
    repo: &str,
    branch: &str,
    has_dockerfile: bool,
    ruby: &str,
) -> Vec<PlanFile> {
    let (dockerfile, workflow, compose, homelab) = match template {
        Template::Rails => (
            include_str!("../templates/rails/Dockerfile"),
            include_str!("../templates/rails/deploy.yml"),
            include_str!("../templates/rails/docker-compose.yml"),
            include_str!("../templates/rails/docker-compose.homelab.yml"),
        ),
        Template::Next => (
            include_str!("../templates/next/Dockerfile"),
            include_str!("../templates/next/deploy.yml"),
            include_str!("../templates/next/docker-compose.yml"),
            include_str!("../templates/next/docker-compose.homelab.yml"),
        ),
    };
    let tech = match template {
        Template::Rails => "ruby",
        Template::Next => "next",
    };
    let render = |text: &str| {
        text.replace("{{slug}}", slug)
            .replace("{{owner}}", owner)
            .replace("{{repo}}", repo)
            .replace("{{branch}}", branch)
            .replace("{{tech}}", tech)
            .replace("{{ruby}}", ruby)
    };
    let mut files = Vec::new();
    if !has_dockerfile {
        files.push(PlanFile { path: "Dockerfile".into(), content: render(dockerfile) });
    }
    files.push(PlanFile { path: ".github/workflows/deploy.yml".into(), content: render(workflow) });
    files.push(PlanFile { path: "deploy/docker-compose.yml".into(), content: render(compose) });
    files.push(PlanFile {
        path: "deploy/docker-compose.homelab.yml".into(),
        content: render(homelab),
    });
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_finds_rails_in_gemfile() {
        let gemfile = "source \"https://rubygems.org\"\ngem \"rails\", \"~> 8.0.2\"\n";
        assert_eq!(detect(Some(gemfile), None), Some(Template::Rails));
    }

    #[test]
    fn detect_finds_next_in_package_json() {
        let pkg = r#"{"dependencies": {"next": "15.1.0", "react": "19.0.0"}}"#;
        assert_eq!(detect(None, Some(pkg)), Some(Template::Next));
        let dev = r#"{"devDependencies": {"next": "15.1.0"}}"#;
        assert_eq!(detect(None, Some(dev)), Some(Template::Next));
    }

    #[test]
    fn detect_rejects_everything_else() {
        assert_eq!(detect(None, None), None);
        assert_eq!(detect(Some("gem \"sinatra\""), None), None);
        assert_eq!(detect(None, Some(r#"{"dependencies": {"react": "19"}}"#)), None);
        assert_eq!(detect(None, Some("not json")), None);
    }

    #[test]
    fn rails_beats_next_when_both_exist() {
        let gemfile = "gem \"rails\"";
        let pkg = r#"{"dependencies": {"next": "15"}}"#;
        assert_eq!(detect(Some(gemfile), Some(pkg)), Some(Template::Rails));
    }

    #[test]
    fn ruby_version_follows_the_repo() {
        // .ruby-version wins, in every spelling it comes in
        assert_eq!(ruby_version(Some("3.2.1\n"), None), "3.2");
        assert_eq!(ruby_version(Some("ruby-3.1.4"), None), "3.1");
        // the Gemfile is the fallback
        let gemfile = "source \"https://rubygems.org\"\nruby \"3.2.1\"\ngem \"rails\"\n";
        assert_eq!(ruby_version(None, Some(gemfile)), "3.2");
        assert_eq!(ruby_version(None, Some("ruby '3.4.2'")), "3.4");
        // the file beats the Gemfile when both are there
        assert_eq!(ruby_version(Some("3.1.0"), Some(gemfile)), "3.1");
        // nothing usable falls back to the template's own
        assert_eq!(ruby_version(None, None), DEFAULT_RUBY);
        assert_eq!(ruby_version(Some(""), Some("gem \"rails\"")), DEFAULT_RUBY);
        assert_eq!(ruby_version(Some("2.7.8"), None), DEFAULT_RUBY, "ancient rubies get the default");
        assert_eq!(ruby_version(Some("garbage"), None), DEFAULT_RUBY);
    }

    #[test]
    fn the_rails_image_is_built_on_the_ruby_the_app_asks_for() {
        let files = plan(Template::Rails, "app", "o", "r", "main", false, "3.2");
        let dockerfile = files.iter().find(|f| f.path == "Dockerfile").unwrap();
        assert!(dockerfile.content.contains("ruby:3.2-slim"), "{}", dockerfile.content);
        assert!(!dockerfile.content.contains("{{ruby}}"));
        // and postgres can actually build: libpq is there on both stages
        assert!(dockerfile.content.contains("libpq-dev"), "the pg gem needs headers to compile");
        assert!(dockerfile.content.contains("libpq5"), "and the runtime needs the client library");
        // migrations and the secret are handled at boot
        assert!(dockerfile.content.contains("db:prepare"));
        assert!(dockerfile.content.contains("SECRET_KEY_BASE"));
    }

    #[test]
    fn plan_renders_placeholders_and_respects_dockerfile() {
        let files = plan(Template::Rails, "axofin", "murichristopher", "axofin", "main", false, "3.3");
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, "Dockerfile");
        let workflow = files.iter().find(|f| f.path == ".github/workflows/deploy.yml").unwrap();
        assert!(workflow.content.contains("branches: [main]"));
        assert!(workflow.content.contains("app: axofin"));
        assert!(workflow.content.contains("${{ secrets.WEBO_DEPLOY_TOKEN }}"), "gh expressions survive");
        let homelab = files.iter().find(|f| f.path == "deploy/docker-compose.homelab.yml").unwrap();
        assert!(homelab.content.contains("ghcr.io/murichristopher/axofin:latest"));
        assert!(homelab.content.contains("webo.tech: ruby"));
        let compose = files.iter().find(|f| f.path == "deploy/docker-compose.yml").unwrap();
        assert!(compose.content.contains("name: axofin"));
        assert!(!compose.content.contains("{{"), "no placeholder left behind");

        // existing Dockerfile is respected
        let files = plan(Template::Next, "landing", "axolutions", "landing", "master", true, "3.3");
        assert_eq!(files.len(), 3);
        assert!(!files.iter().any(|f| f.path == "Dockerfile"));
        let workflow = files.iter().find(|f| f.path == ".github/workflows/deploy.yml").unwrap();
        assert!(workflow.content.contains("branches: [master]"));
    }
}
