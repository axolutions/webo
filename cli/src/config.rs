use crate::{Failure, fail};
use serde_json::{Value, json};

pub const PATH: &str = "~/.config/webo/config";

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub default: Option<String>,
    pub sections: Vec<Section>,
}

#[derive(Debug, PartialEq)]
pub struct Section {
    pub name: String,
    pub values: Vec<(String, String)>,
}

impl Section {
    fn get(&self, key: &str) -> Option<String> {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub name: String,
    pub url: String,
    pub token: String,
}

pub fn parse(text: &str) -> Config {
    let mut config = Config::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            config.sections.push(Section {
                name: name.trim().to_string(),
                values: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_string(), value.trim().to_string());
        match config.sections.last_mut() {
            Some(section) => section.values.push((key, value)),
            None if key == "default" => config.default = Some(value),
            None => {}
        }
    }
    config
}

pub fn resolve(
    config: &Config,
    requested: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<Server, Failure> {
    let explicit = requested.is_some();
    let name = requested
        .map(str::to_string)
        .or_else(|| var("WEBO_SERVER"))
        .or_else(|| config.default.clone())
        .or_else(|| config.sections.first().map(|s| s.name.clone()));
    let section = name
        .as_deref()
        .and_then(|n| config.sections.iter().find(|s| s.name == n));
    let env = |key: &str| if explicit { None } else { var(key) };

    if let (Some(n), None) = (&name, section)
        && (explicit || env("WEBO_URL").is_none())
    {
        return Err(fail(
            "config",
            format!(
                "no server '{n}' in {PATH}; known: {}",
                known(config).unwrap_or_else(|| "none".into())
            ),
        ));
    }

    let url = env("WEBO_URL").or_else(|| section.and_then(|s| s.get("url")));
    let token = env("WEBO_TOKEN").or_else(|| section.and_then(|s| s.get("token")));
    let name = name.unwrap_or_else(|| "default".into());
    match (url, token) {
        (Some(url), Some(token)) if !url.is_empty() && !token.is_empty() => {
            Ok(Server { name, url, token })
        }
        (None, _) if config.sections.is_empty() => Err(fail(
            "config",
            format!(
                "no server configured: add a [name] block with url and token to {PATH}, or set WEBO_URL and WEBO_TOKEN"
            ),
        )),
        (url, _) => Err(fail(
            "config",
            format!(
                "server '{name}' needs {} in {PATH}",
                if url.is_none() { "a url" } else { "a token" }
            ),
        )),
    }
}

fn known(config: &Config) -> Option<String> {
    if config.sections.is_empty() {
        return None;
    }
    Some(
        config
            .sections
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

pub fn describe(config: &Config, var: &dyn Fn(&str) -> Option<String>) -> Value {
    let default = resolve(config, None, var).ok().map(|s| s.name);
    let servers: Vec<Value> = config
        .sections
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "url": s.get("url"),
                "token": s.get("token").is_some_and(|t| !t.is_empty()),
            })
        })
        .collect();
    json!({"ok": true, "config": PATH, "default": default, "servers": servers})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::CONFIG;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn parses_blocks_and_default() {
        let config =
            parse("# comment\ndefault = b\n[a]\nurl = u1\n\nnoise\n[ b ]\ntoken = t = x\n");
        assert_eq!(config.default.as_deref(), Some("b"));
        assert_eq!(config.sections.len(), 2);
        assert_eq!(config.sections[1].name, "b");
        assert_eq!(config.sections[1].get("token").as_deref(), Some("t = x"));
    }

    #[test]
    fn resolves_default_requested_and_env() {
        let config = parse(CONFIG);
        assert_eq!(resolve(&config, None, &none).unwrap().name, "homelab");
        let vps = resolve(&config, Some("vps"), &none).unwrap();
        assert_eq!(
            (vps.url.as_str(), vps.token.as_str()),
            ("https://vps.example/mcp", "vps-token")
        );
        let via_env = |k: &str| (k == "WEBO_SERVER").then(|| "vps".to_string());
        assert_eq!(resolve(&config, None, &via_env).unwrap().name, "vps");
    }

    #[test]
    fn env_overrides_only_without_an_explicit_server() {
        let config = parse(CONFIG);
        let env = |k: &str| match k {
            "WEBO_URL" => Some("https://env/mcp".to_string()),
            "WEBO_TOKEN" => Some("env-token".to_string()),
            _ => None,
        };
        assert_eq!(resolve(&config, None, &env).unwrap().url, "https://env/mcp");
        assert_eq!(
            resolve(&config, Some("vps"), &env).unwrap().url,
            "https://vps.example/mcp"
        );
        let only_env = resolve(&Config::default(), None, &env).unwrap();
        assert_eq!(
            (only_env.name.as_str(), only_env.token.as_str()),
            ("default", "env-token")
        );
    }

    #[test]
    fn explains_what_is_missing() {
        let config = parse(CONFIG);
        let err = resolve(&config, Some("nope"), &none).unwrap_err();
        assert!(err.message.contains("known: homelab, vps"));
        let err = resolve(&Config::default(), None, &none).unwrap_err();
        assert!(err.message.contains("no server configured"));
        let err = resolve(&Config::default(), Some("x"), &none).unwrap_err();
        assert!(err.message.contains("known: none"));
        let err = resolve(&parse("[a]\nurl = u\n"), None, &none).unwrap_err();
        assert!(err.message.contains("needs a token"));
        let err = resolve(&parse("[a]\ntoken = t\n"), None, &none).unwrap_err();
        assert!(err.message.contains("needs a url"));
    }

    #[test]
    fn describe_hides_tokens() {
        let out = describe(&parse(CONFIG), &none);
        assert_eq!(out["servers"][0]["token"], true);
        assert!(!out.to_string().contains("home-token"));
    }
}
