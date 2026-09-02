//! Error tracking: the baseline comes from the logs webo already indexes,
//! so every app gets server-side error grouping without installing anything.
//! A tiny optional snippet covers what the server never sees — the errors
//! that happen in the visitor's browser.

use serde::{Deserialize, Serialize};

/// Signatures that mark a log line as an error worth grouping.
const MARKERS: [&str; 10] = [
    "error", "exception", "panic:", "traceback", "fatal",
    "unhandled", "uncaught", "segfault", "err!", "critical",
];

/// Lines that merely *mention* an error without being one — noisy in
/// databases and proxies, and grouping them buries the real thing.
const NOISE: [&str; 6] = [
    "log:  checkpoint",
    "error_log",
    "0 errors",
    "no errors",
    "errorlog",
    "error_reporting",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorEvent {
    pub title: String,
    pub message: String,
    /// "server" or "browser"
    pub source: String,
    /// container name, or the page URL for browser errors
    pub origin: String,
    pub ts: i64,
}

/// Many loggers stamp their own level into the line. Ruby/Rails writes
/// `E, [2026-09-02T00:49:30 #1] ERROR -- : the message`, and that declared
/// level is authoritative: an INFO line that merely says "500 Internal Server
/// Error" is not an error, and grouping it buries the real one.
/// Returns (declared level, the message without the prefix).
pub fn strip_log_prefix(line: &str) -> (Option<&'static str>, &str) {
    let t = line.trim_start();
    // Ruby Logger: "<L>, [<timestamp> #<pid>] <LEVEL> -- : <message>"
    if let Some(rest) = t.split_once("] ").map(|(_, r)| r) {
        if t.starts_with(['D', 'I', 'W', 'E', 'F', 'A']) && t[1..].starts_with(", [") {
            let (level_word, msg) = match rest.split_once(" -- : ") {
                Some(pair) => pair,
                None => return (None, line),
            };
            let level = match level_word.trim() {
                "ERROR" | "FATAL" => "error",
                "WARN" => "warn",
                _ => "info",
            };
            return (Some(level), msg);
        }
    }
    (None, line)
}

/// Is there a message here, or only punctuation and whitespace?
fn has_substance(msg: &str) -> bool {
    msg.chars().any(|c| c.is_alphanumeric())
}

/// Rails prefixes every line of a request with its id — unique per request,
/// so it must never reach a title or a fingerprint.
pub fn strip_request_id(msg: &str) -> &str {
    let t = msg.trim_start();
    let Some(inner) = t.strip_prefix('[') else { return t };
    let Some((id, rest)) = inner.split_once(']') else { return t };
    let looks_like_id = id.len() >= 8
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && id.chars().any(|c| c.is_ascii_digit());
    if looks_like_id { rest.trim_start() } else { t }
}

/// Log level, derived from the line itself — nothing is stored for this,
/// the heuristic is cheap enough to run at read time.
pub fn level_of(line: &str, stream: &str) -> &'static str {
    // a line that states its own level is believed
    if let (Some(declared), _) = strip_log_prefix(line) {
        return declared;
    }
    if looks_like_error(line, stream) {
        return "error";
    }
    let lower = line.to_ascii_lowercase();
    if lower.contains("warn") || lower.contains("deprecat") {
        "warn"
    } else {
        "info"
    }
}

/// Does this line belong to the stack of the error above it?
pub fn is_stack_frame(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("at ") || t.starts_with("File \"") || t.starts_with("Caused by")
        || t.starts_with("from ") && line.starts_with(' ')
}

/// First stack frame's location — the file to blame, when the error carried
/// a stack. `at handler (app/api/route.ts:31:5)` → `app/api/route.ts:31:5`.
pub fn culprit_of(message: &str) -> Option<String> {
    for line in message.lines().skip(1) {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("at ") {
            let place = match (rest.rfind('('), rest.rfind(')')) {
                (Some(a), Some(b)) if a < b => &rest[a + 1..b],
                _ => rest,
            };
            let place = place.trim();
            if !place.is_empty() {
                return Some(place.chars().take(160).collect());
            }
        }
        if let Some(rest) = t.strip_prefix("File \"") {
            let file = rest.split('"').next().unwrap_or(rest);
            let line_no = rest.split("line ").nth(1).and_then(|x| x.split([',', ' ']).next());
            return Some(match line_no {
                Some(n) => format!("{file}:{n}"),
                None => file.to_string(),
            });
        }
    }
    None
}

/// Is this log line an error?
pub fn looks_like_error(line: &str, stream: &str) -> bool {
    // a logger that states its own level settles it: an INFO line reading
    // "Completed 500 Internal Server Error" is a status, not a new error
    if let (Some(declared), rest) = strip_log_prefix(line) {
        // Rails ends an exception report with a bare ERROR line — a prefix,
        // a request id and nothing else. It is spacing, not an occurrence.
        return declared == "error" && has_substance(strip_request_id(rest));
    }
    let lower = line.to_ascii_lowercase();
    if NOISE.iter().any(|n| lower.contains(n)) {
        return false;
    }
    // a stack frame belongs to the error above it, not to a new one
    let trimmed = line.trim_start();
    if trimmed.starts_with("at ") || trimmed.starts_with("File \"") {
        return false;
    }
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // stderr alone is not enough: plenty of tools log status to stderr
    stream == "stderr" && lower.contains("failed")
}

/// Normalizes a message so two occurrences of the same bug land on the same
/// issue: digits, hex, uuids, quoted text and paths become placeholders.
pub fn fingerprint(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars().peekable();
    let mut in_quote = false;
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                if !in_quote {
                    out.push_str("<str>");
                }
                in_quote = !in_quote;
            }
            _ if in_quote => {}
            '/' => {
                // collapse a path into a single placeholder
                while chars.peek().is_some_and(|n| !n.is_whitespace()) {
                    chars.next();
                }
                out.push_str("<path>");
            }
            c if c.is_ascii_digit() => {
                // a run of hex and dashes is an id (uuid, request id, sha)
                let mut run = String::from(c);
                while chars
                    .peek()
                    .is_some_and(|n| n.is_ascii_hexdigit() || matches!(n, '.' | ':' | '-'))
                {
                    run.push(chars.next().unwrap());
                }
                let hexish = run.len() >= 8 && run.chars().any(|c| c.is_ascii_alphabetic());
                out.push_str(if hexish { "<id>" } else { "<n>" });
            }
            c if c.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            c => out.push(c.to_ascii_lowercase()),
        }
    }
    out.trim().chars().take(200).collect()
}

/// A short human title: the first line, trimmed of timestamps and levels.
pub fn title_of(message: &str) -> String {
    let first = message.lines().next().unwrap_or(message).trim();
    // a structured logger's own prefix goes first, then the request id
    let (_, without_prefix) = strip_log_prefix(first);
    let first = strip_request_id(without_prefix);
    // then a plain "ERROR:" style prefix, e.g. "2026-09-01 03:11 UTC [117] ERROR:  x"
    let cleaned = first
        .split_once("ERROR:")
        .or_else(|| first.split_once("Error:"))
        .or_else(|| first.split_once("error:"))
        .map(|(_, rest)| rest.trim())
        .unwrap_or(first);
    cleaned.chars().take(160).collect()
}

/// What the ingest endpoint accepts from the browser snippet.
#[derive(Debug, Deserialize)]
pub struct BrowserReport {
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

impl BrowserReport {
    pub fn into_event(self, ts: i64) -> ErrorEvent {
        let kind = self.kind.unwrap_or_else(|| "error".into());
        let message = match &self.stack {
            Some(s) if !s.is_empty() => format!("{}\n{}", self.message, s),
            _ => self.message.clone(),
        };
        ErrorEvent {
            title: title_of(&format!("{kind}: {}", self.message)),
            message,
            source: "browser".into(),
            origin: self.url.unwrap_or_default(),
            ts,
        }
    }
}

/// The snippet the user pastes (or the template injects): hooks uncaught
/// errors, rejected promises and failed resources.
pub fn snippet(base_url: &str, key: &str) -> String {
    format!(
        // text/plain keeps it a CORS-simple request: application/json would
        // demand a preflight, and sendBeacon cannot preflight — it just fails
        r#"<script>(function(){{var u="{base_url}/api/v1/ingest/{key}";function s(m,k,st){{try{{var b=JSON.stringify({{message:m,kind:k,stack:st,url:location.href}});navigator.sendBeacon?navigator.sendBeacon(u,new Blob([b],{{type:"text/plain;charset=UTF-8"}})):fetch(u,{{method:"POST",headers:{{"content-type":"text/plain;charset=UTF-8"}},body:b,keepalive:true}})}}catch(e){{}}}}
window.addEventListener("error",function(e){{e.error?s(String(e.error.message||e.message),"error",e.error.stack):s("failed to load "+((e.target&&(e.target.src||e.target.href))||"resource"),"resource")}},true);
window.addEventListener("unhandledrejection",function(e){{var r=e.reason||{{}};s(String(r.message||r),"unhandledrejection",r.stack)}});}})();</script>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_told_apart_from_ordinary_lines() {
        assert!(looks_like_error("ERROR: syntax error at or near \"1\"", "stderr"));
        assert!(looks_like_error("Uncaught TypeError: x is not a function", "stdout"));
        assert!(looks_like_error("panic: runtime error: index out of range", "stderr"));
        assert!(looks_like_error("Traceback (most recent call last):", "stderr"));
        assert!(looks_like_error("connection failed after 3 tries", "stderr"));

        // ordinary lines stay out
        assert!(!looks_like_error("GET /health 200", "stdout"));
        assert!(!looks_like_error("LOG:  checkpoint complete: wrote 9 buffers", "stderr"));
        assert!(!looks_like_error("listening on :3000", "stdout"));
        // a stack frame belongs to the error above it
        assert!(!looks_like_error("    at Object.<anonymous> (/app/lib/db.ts:12:9)", "stderr"));
        assert!(!looks_like_error("  File \"/app/main.py\", line 3", "stderr"));
    }

    #[test]
    fn the_same_bug_lands_on_the_same_fingerprint() {
        let a = fingerprint("ERROR: connection to 10.0.0.7:5432 refused for user \"leads\"");
        let b = fingerprint("ERROR: connection to 192.168.1.22:5432 refused for user \"admin\"");
        assert_eq!(a, b, "addresses and quoted values must not split an issue");

        let c = fingerprint("Cannot read properties of undefined (reading 'nome')");
        let d = fingerprint("Cannot read properties of undefined (reading 'email')");
        assert_eq!(c, d);

        // genuinely different problems stay apart
        assert_ne!(fingerprint("connection refused"), fingerprint("permission denied"));
        // paths collapse
        assert_eq!(
            fingerprint("failed to open /app/data/2026/file.txt"),
            fingerprint("failed to open /var/lib/other.txt")
        );
    }

    #[test]
    fn titles_drop_the_noise_before_the_message() {
        assert_eq!(
            title_of("2026-09-01 03:11:04.790 UTC [117] ERROR:  syntax error at or near \"1\""),
            "syntax error at or near \"1\""
        );
        assert_eq!(title_of("Error: connection refused\n  at db.ts:1"), "connection refused");
        assert_eq!(title_of("plain message"), "plain message");
        assert!(title_of(&"x".repeat(500)).chars().count() <= 160);
    }

    #[test]
    fn the_same_bug_reported_in_two_formats_is_one_issue() {
        // the app logs it with its own prefix; the framework logs its own line
        let mine = "[webo-check] TypeError na rota /api/quebra: TypeError: Cannot read properties of null (reading 'valor')";
        let framework = "TypeError: Cannot read properties of null (reading 'valor')";
        assert_eq!(
            fingerprint(&title_of(mine)),
            fingerprint(&title_of(framework)),
            "one bug, one issue"
        );
    }

    #[test]
    fn a_rails_request_is_one_issue_not_a_dozen() {
        // what a single failing Rails request writes: the app's own line, the
        // framework's status line, and the exception with its request id
        let lines = [
            "E, [2026-09-02T00:49:29.029053 #1] ERROR -- : [67737fac-3b15-4ed1-914a-7e1f5e6ca722] [railsdemo] about to explode on purpose",
            "I, [2026-09-02T00:49:29.029174 #1]  INFO -- : [67737fac-3b15-4ed1-914a-7e1f5e6ca722] Completed 500 Internal Server Error in 0ms",
            "[67737fac-3b15-4ed1-914a-7e1f5e6ca722] NoMethodError (undefined method `valor' for nil:NilClass):",
        ];
        // the INFO status line is not an error, however much it says "Error"
        assert!(!looks_like_error(lines[1], "stdout"), "a status line is not a new error");
        // and Rails closes an exception report with an empty ERROR line
        assert!(
            !looks_like_error("E, [2026-09-02T00:57:26.126798 #1] ERROR -- : [4def44af-9dc0-44f9-90c0-ff7e8aaca782]   ", "stdout"),
            "a prefix with no message is spacing, not an occurrence"
        );
        assert_eq!(level_of(lines[1], "stdout"), "info", "the logger's own level wins");
        assert!(looks_like_error(lines[0], "stdout"));
        assert_eq!(level_of(lines[0], "stdout"), "error");

        // titles carry neither the logger prefix nor the request id
        assert_eq!(title_of(lines[0]), "[railsdemo] about to explode on purpose");
        assert_eq!(
            title_of(lines[2]),
            "NoMethodError (undefined method `valor' for nil:NilClass):"
        );

        // and the SAME failure on another request lands on the same issue
        let other_request = "[3769503f-467d-4085-a50b-db9ec9249fb1] NoMethodError (undefined method `valor' for nil:NilClass):";
        assert_eq!(
            fingerprint(&title_of(lines[2])),
            fingerprint(&title_of(other_request)),
            "the request id must not split one bug into two issues"
        );
        // two genuinely different failures stay apart
        let db = "[abc12345-0000-0000-0000-000000000000] ActiveRecord::StatementInvalid (PG::UndefinedColumn: ERROR:  column \"x\" does not exist)";
        assert_ne!(fingerprint(&title_of(lines[2])), fingerprint(&title_of(db)));
    }

    #[test]
    fn log_prefixes_and_request_ids_are_recognised() {
        let (lvl, msg) = strip_log_prefix("W, [2026-09-02T00:00:00 #1]  WARN -- : cache miss");
        assert_eq!(lvl, Some("warn"));
        assert_eq!(msg, "cache miss");
        let (lvl, msg) = strip_log_prefix("F, [2026-09-02T00:00:00 #1] FATAL -- : boom");
        assert_eq!(lvl, Some("error"), "fatal counts as an error");
        assert_eq!(msg, "boom");
        // a plain line is left alone
        let (lvl, msg) = strip_log_prefix("GET /health 200");
        assert_eq!(lvl, None);
        assert_eq!(msg, "GET /health 200");

        assert_eq!(strip_request_id("[67737fac-3b15-4ed1-914a-7e1f5e6ca722] boom"), "boom");
        assert_eq!(strip_request_id("[railsdemo] listing notes"), "[railsdemo] listing notes",
                   "a word in brackets is not a request id");
        assert_eq!(strip_request_id("no brackets"), "no brackets");
    }

    #[test]
    fn levels_are_derived_not_stored() {
        assert_eq!(level_of("ERROR: boom", "stderr"), "error");
        assert_eq!(level_of("WARN cache miss em leads:sp", "stdout"), "warn");
        assert_eq!(level_of("DeprecationWarning: punycode", "stdout"), "warn");
        assert_eq!(level_of("GET /health 200", "stdout"), "info");
        assert_eq!(level_of("listening on :3000", "stderr"), "info");
    }

    #[test]
    fn stack_frames_are_recognized_and_blamed() {
        assert!(is_stack_frame("    at w (.next/server/app/api/quebra/route.js:1:823)"));
        assert!(is_stack_frame("  File \"/app/main.py\", line 3, in <module>"));
        assert!(!is_stack_frame("GET / 200"));
        assert!(!is_stack_frame("TypeError: x"));

        let msg = "TypeError: Cannot read properties of null (reading 'valor')\n    at w (.next/server/app/api/quebra/route.js:1:823)\n    at async (node:internal)";
        assert_eq!(culprit_of(msg).as_deref(), Some(".next/server/app/api/quebra/route.js:1:823"));

        let py = "Traceback (most recent call last):\n  File \"/app/main.py\", line 3, in <module>\n    boom()";
        assert_eq!(culprit_of(py).as_deref(), Some("/app/main.py:3"));

        assert_eq!(culprit_of("erro sem stack"), None);
        // frame without parens still yields a place
        assert_eq!(culprit_of("x\n    at db.ts:1:2").as_deref(), Some("db.ts:1:2"));
    }

    #[test]
    fn browser_reports_become_events() {
        let r = BrowserReport {
            message: "x is not a function".into(),
            url: Some("https://app.example.com/checkout".into()),
            stack: Some("at pay (checkout.js:10)".into()),
            kind: Some("TypeError".into()),
        };
        let e = r.into_event(1000);
        assert_eq!(e.source, "browser");
        assert_eq!(e.origin, "https://app.example.com/checkout");
        assert!(e.message.contains("at pay"));
        assert!(e.title.contains("x is not a function"));
    }

    #[test]
    fn the_snippet_carries_the_endpoint_and_hooks_everything() {
        let s = snippet("https://webo.example.com", "abc123");
        assert!(s.contains("https://webo.example.com/api/v1/ingest/abc123"));
        assert!(s.contains("unhandledrejection"));
        assert!(s.contains("sendBeacon"));
        assert!(s.contains("text/plain"), "json would need a preflight a beacon cannot do");
        assert!(!s.contains("application/json"));
        assert!(s.starts_with("<script>") && s.ends_with("</script>"));
    }
}
