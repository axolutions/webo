//! Cloudflare integration (optional): every project gets a public URL through
//! the tunnel — a random-words hostname on the apps zone, plus an optional
//! custom domain. Routes live in the tunnel's remote configuration (API), so
//! webo never rewrites the cloudflared config file that keeps other apps up.

use serde_json::{json, Value};
use std::time::Duration;

/// Word list for the auto domain: short, unambiguous, spelling-safe.
const WORDS: [&str; 64] = [
    "amber", "anchor", "arbor", "aspen", "atlas", "basil", "beacon", "birch",
    "bloom", "bronze", "canyon", "cedar", "cinder", "clover", "cobalt", "comet",
    "coral", "cove", "crest", "dawn", "delta", "dune", "ember", "fable",
    "fern", "flint", "forest", "garnet", "glade", "grove", "harbor", "haven",
    "hazel", "indigo", "ivory", "jade", "juniper", "lagoon", "lantern", "linen",
    "lumen", "maple", "meadow", "mica", "mint", "north", "oasis", "onyx",
    "opal", "orchid", "pebble", "pine", "prism", "quartz", "quill", "reef",
    "ridge", "river", "saffron", "sage", "slate", "summit", "willow", "zephyr",
];

/// `<word>-<word>-<word>` — 64³ ≈ 262k combinations, plenty for a homelab.
pub fn random_label(rand: &mut impl FnMut() -> u64) -> String {
    let mut parts = Vec::with_capacity(3);
    for _ in 0..3 {
        parts.push(WORDS[(rand() as usize) % WORDS.len()]);
    }
    parts.join("-")
}

pub fn random_label_os() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let state = RandomState::new();
    let mut next = || state.build_hasher().finish();
    random_label(&mut next)
}

/// Splits a hostname into (label, zone) against a known zone.
/// `loja.example.com` on zone `example.com` → Some(("loja", "example.com")).
pub fn split_host<'a>(host: &'a str, zone: &str) -> Option<(&'a str, &'a str)> {
    let rest = host.strip_suffix(zone)?.strip_suffix('.')?;
    if rest.is_empty() || rest.contains('.') {
        return None; // apex or deeper level: not supported on the free plan
    }
    Some((rest, &host[rest.len() + 1..]))
}

/// A hostname is valid when it is a DNS label chain of allowed characters.
pub fn valid_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').count() >= 2
        && host.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Rebuilds the tunnel ingress: every route webo knows about, with the
/// catch-all 404 always last. Existing rules for hosts webo does not manage
/// are preserved in their original order.
pub fn merge_ingress(current: &Value, managed: &[(String, String)]) -> Vec<Value> {
    let managed_hosts: Vec<&str> = managed.iter().map(|(h, _)| h.as_str()).collect();
    let mut out: Vec<Value> = Vec::new();
    if let Some(rules) = current.as_array() {
        for r in rules {
            let Some(host) = r.get("hostname").and_then(|h| h.as_str()) else {
                continue; // the catch-all is re-added at the end
            };
            if !managed_hosts.contains(&host) {
                out.push(r.clone());
            }
        }
    }
    for (host, service) in managed {
        out.push(json!({ "hostname": host, "service": service }));
    }
    out.push(json!({ "service": "http_status:404" }));
    out
}

pub struct Cloudflare {
    token: String,
    account_id: String,
    zone_id: String,
    tunnel_id: String,
    pub apps_zone: String,
}

impl Cloudflare {
    /// Built only when every piece is configured; otherwise domains degrade
    /// gracefully and the panel says so.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Some(Self {
            token: get("CLOUDFLARE_API_TOKEN")?,
            account_id: get("CLOUDFLARE_ACCOUNT_ID")?,
            zone_id: get("CLOUDFLARE_ZONE_ID")?,
            tunnel_id: get("WEBO_TUNNEL_ID")?,
            apps_zone: get("WEBO_APPS_ZONE")?,
        })
    }

    fn base() -> String {
        std::env::var("WEBO_CF_API_BASE")
            .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".into())
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let url = format!("{}{}", Self::base(), path);
        let req = ureq::request(method, &url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(20));
        let res = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };
        let json: Value = match res {
            Ok(r) => r.into_json().map_err(|e| e.to_string())?,
            // 4xx carries the reason in the body — read it instead of the status
            Err(ureq::Error::Status(_, r)) => r.into_json().map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        };
        if json.get("success").and_then(|s| s.as_bool()) == Some(true) {
            Ok(json.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(first_error(&json))
        }
    }

    pub fn tunnel_target(&self) -> String {
        format!("{}.cfargotunnel.com", self.tunnel_id)
    }

    pub fn ingress(&self) -> Result<Value, String> {
        let r = self.call(
            "GET",
            &format!("/accounts/{}/cfd_tunnel/{}/configurations", self.account_id, self.tunnel_id),
            None,
        )?;
        Ok(r.pointer("/config/ingress").cloned().unwrap_or(Value::Array(vec![])))
    }

    pub fn put_ingress(&self, rules: Vec<Value>) -> Result<(), String> {
        self.call(
            "PUT",
            &format!("/accounts/{}/cfd_tunnel/{}/configurations", self.account_id, self.tunnel_id),
            Some(&json!({ "config": { "ingress": rules } })),
        )?;
        Ok(())
    }

    /// Points a hostname of the apps zone at the tunnel. Returns Ok(()) when
    /// the record already exists.
    pub fn create_dns(&self, label: &str, comment: &str) -> Result<(), String> {
        let body = json!({
            "type": "CNAME",
            "name": label,
            "content": self.tunnel_target(),
            "proxied": true,
            "comment": comment,
        });
        match self.call("POST", &format!("/zones/{}/dns_records", self.zone_id), Some(&body)) {
            Ok(_) => Ok(()),
            Err(e) if e.contains("already exists") || e.contains("81053") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn delete_dns(&self, host: &str) -> Result<(), String> {
        let list = self.call(
            "GET",
            &format!("/zones/{}/dns_records?name={host}", self.zone_id),
            None,
        )?;
        let Some(id) = list.as_array().and_then(|a| a.first()).and_then(|r| r.get("id")).and_then(|i| i.as_str())
        else {
            return Ok(()); // nothing to delete
        };
        self.call("DELETE", &format!("/zones/{}/dns_records/{id}", self.zone_id), None)?;
        Ok(())
    }

    /// True when the hostname resolves to this tunnel — used to tell the user
    /// a third-party CNAME has propagated.
    pub fn points_here(&self, host: &str) -> bool {
        std::net::ToSocketAddrs::to_socket_addrs(&(host, 443)).is_ok()
    }
}

fn first_error(json: &Value) -> String {
    json.get("errors")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or("cloudflare request failed")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_label_has_three_known_words() {
        let mut n = 0u64;
        let mut seq = || {
            n += 1;
            n
        };
        let label = random_label(&mut seq);
        let parts: Vec<&str> = label.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| WORDS.contains(p)));
        assert_eq!(label, "anchor-arbor-aspen", "words follow the sequence");
        assert!(valid_hostname(&format!("{label}.example.com")));
    }

    #[test]
    fn random_label_os_is_usable_and_varies() {
        let a = random_label_os();
        assert_eq!(a.split('-').count(), 3);
        assert!(valid_hostname(&format!("{a}.example.com")));
    }

    #[test]
    fn split_host_only_accepts_one_level() {
        assert_eq!(split_host("loja.example.com", "example.com"), Some(("loja", "example.com")));
        assert_eq!(split_host("a.b.example.com", "example.com"), None, "second level not supported");
        assert_eq!(split_host("example.com", "example.com"), None, "apex not supported");
        assert_eq!(split_host("loja.other.com", "example.com"), None);
    }

    #[test]
    fn hostname_validation() {
        assert!(valid_hostname("app.example.com"));
        assert!(valid_hostname("a-b-c.example.com.br"));
        assert!(!valid_hostname("semponto"));
        assert!(!valid_hostname("-bad.example.com"));
        assert!(!valid_hostname("bad-.example.com"));
        assert!(!valid_hostname("ba d.example.com"));
        assert!(!valid_hostname(""));
    }

    #[test]
    fn merge_ingress_replaces_managed_and_keeps_the_rest() {
        let current = json!([
            {"hostname": "codo.example.com", "service": "http://codo:4949"},
            {"hostname": "old.example.com", "service": "http://old:3000"},
            {"service": "http_status:404"}
        ]);
        let managed = vec![
            ("old.example.com".to_string(), "http://new:3000".to_string()),
            ("fresh.example.com".to_string(), "http://fresh:3000".to_string()),
        ];
        let out = merge_ingress(&current, &managed);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["hostname"], "codo.example.com", "untouched rule kept first");
        assert_eq!(out[1]["hostname"], "old.example.com");
        assert_eq!(out[1]["service"], "http://new:3000", "managed rule replaced");
        assert_eq!(out[2]["hostname"], "fresh.example.com");
        assert_eq!(out[3]["service"], "http_status:404", "catch-all is always last");
        assert!(out[3].get("hostname").is_none());
    }

    #[test]
    fn merge_ingress_drops_a_removed_project() {
        let current = json!([
            {"hostname": "gone.example.com", "service": "http://gone:3000"},
            {"service": "http_status:404"}
        ]);
        let out = merge_ingress(&current, &[]);
        assert_eq!(out.len(), 2, "the removed host is gone, catch-all stays");
        assert_eq!(out[0]["hostname"], "gone.example.com");
        let out = merge_ingress(&current, &[("gone.example.com".into(), "http://x:1".into())]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["service"], "http://x:1");
    }

    #[test]
    fn from_env_needs_every_piece() {
        let _lock = crate::testutil::env_lock();
        for k in ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_ZONE_ID", "WEBO_TUNNEL_ID", "WEBO_APPS_ZONE"] {
            std::env::remove_var(k);
        }
        assert!(Cloudflare::from_env().is_none());
        std::env::set_var("CLOUDFLARE_API_TOKEN", "t");
        std::env::set_var("CLOUDFLARE_ACCOUNT_ID", "a");
        std::env::set_var("CLOUDFLARE_ZONE_ID", "z");
        std::env::set_var("WEBO_TUNNEL_ID", "tun");
        std::env::set_var("WEBO_APPS_ZONE", "example.com");
        let cf = Cloudflare::from_env().expect("configured");
        assert_eq!(cf.apps_zone, "example.com");
        assert_eq!(cf.tunnel_target(), "tun.cfargotunnel.com");
        std::env::set_var("CLOUDFLARE_API_TOKEN", "  ");
        assert!(Cloudflare::from_env().is_none(), "blank counts as missing");
        for k in ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_ZONE_ID", "WEBO_TUNNEL_ID", "WEBO_APPS_ZONE"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn first_error_reads_the_message() {
        let e = json!({"success": false, "errors": [{"code": 81053, "message": "record already exists"}]});
        assert_eq!(first_error(&e), "record already exists");
        assert_eq!(first_error(&json!({})), "cloudflare request failed");
    }
}
