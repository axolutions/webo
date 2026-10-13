use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

const SKILL: &str = include_str!("../docs/skills/webo/SKILL.md");
const GUIDE: &str = include_str!("../docs/install.md");

pub struct Instance {
    pub base: String,
    pub host: String,
    pub name: String,
}

pub fn instance(headers: &HeaderMap) -> Instance {
    let configured = std::env::var("WEBO_PUBLIC_URL").ok().filter(|v| !v.trim().is_empty());
    let base = match configured {
        Some(url) => url.trim().trim_end_matches('/').to_string(),
        None => {
            let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("localhost:5050");
            let local = host.starts_with("localhost") || host.starts_with("127.");
            format!("{}://{host}", if local { "http" } else { "https" })
        }
    };
    let host = base.split("://").nth(1).unwrap_or(&base).split('/').next().unwrap_or_default().to_string();
    let name = std::env::var("WEBO_NAME")
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or_else(|| "webo".into());
    Instance { base, host, name }
}

pub fn render(template: &str, i: &Instance) -> String {
    template.replace("{BASE}", &i.base).replace("{HOST}", &i.host).replace("{NAME}", &i.name)
}

// Streamable HTTP clients probe GET /mcp for an event stream and expect 405.
pub async fn mcp_get(headers: HeaderMap) -> Response {
    let wants_stream = headers
        .get(header::ACCEPT)
        .and_then(|a| a.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"));
    if wants_stream {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    }
    markdown(render(GUIDE, &instance(&headers)))
}

pub async fn skill(headers: HeaderMap) -> Response {
    markdown(render(SKILL, &instance(&headers)))
}

fn markdown(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/markdown; charset=utf-8")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, host.parse().unwrap());
        h
    }

    fn clear_env() {
        std::env::remove_var("WEBO_PUBLIC_URL");
        std::env::remove_var("WEBO_NAME");
    }

    #[test]
    fn without_configuration_the_address_comes_from_the_request() {
        let _env = crate::testutil::env_lock();
        clear_env();
        let i = instance(&headers("panel.example.com"));
        assert_eq!(i.base, "https://panel.example.com");
        assert_eq!(i.host, "panel.example.com");
        assert_eq!(i.name, "webo");
        assert_eq!(instance(&headers("localhost:5050")).base, "http://localhost:5050");
        assert_eq!(instance(&HeaderMap::new()).base, "http://localhost:5050");
    }

    #[test]
    fn the_configured_address_and_name_win() {
        let _env = crate::testutil::env_lock();
        std::env::set_var("WEBO_PUBLIC_URL", "https://ops.example.com/");
        std::env::set_var("WEBO_NAME", "ops");
        let i = instance(&headers("10.0.0.5:5050"));
        std::env::set_var("WEBO_NAME", "no spaces allowed");
        let bad = instance(&headers("10.0.0.5:5050"));
        clear_env();
        assert_eq!(i.base, "https://ops.example.com");
        assert_eq!(i.host, "ops.example.com");
        assert_eq!(i.name, "ops");
        assert_eq!(bad.name, "webo");
    }

    #[test]
    fn guide_and_skill_point_at_this_instance_and_leave_no_placeholder() {
        let i = Instance { base: "https://ops.example.com".into(), host: "ops.example.com".into(), name: "ops".into() };
        let guide = render(GUIDE, &i);
        let skill = render(SKILL, &i);
        for doc in [&guide, &skill] {
            for placeholder in ["{BASE}", "{HOST}", "{NAME}"] {
                assert!(!doc.contains(placeholder), "{placeholder} left behind");
            }
        }
        assert!(guide.contains("curl -s -X POST https://ops.example.com/api/device/start"));
        assert!(guide.contains("https://ops.example.com/authorize?code="));
        assert!(guide.contains("--transport http ops https://ops.example.com/mcp"));
        assert!(guide.contains("https://ops.example.com/skill.md"));
        assert!(skill.starts_with("---\nname: ops\n"));
        assert!(skill.contains("mcp__ops__"));
    }
}
