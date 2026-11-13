use crate::Failure;
use crate::args::usage;
use crate::config::{Config, PATH};
use crate::schema::{Tool, is_secret, kebab};
use serde_json::Value;

const HEAD: &str = "webo — command line for a webo server, through its own /mcp endpoint

usage: webo [--server <name>] <command> [arguments] [--yes | --dry-run]

global flags:
  -s, --server <name>  server block from the config (default: 'default' key, else the first block)
  -y, --yes            run a command that changes something on the server
      --dry-run        print what would be sent, send nothing
  -h, --help           this help; '<command> --help' shows a command's parameters
";

fn first_sentence(text: &str) -> String {
    let sentence = text
        .split_once(". ")
        .map(|(first, _)| first)
        .unwrap_or(text)
        .trim_end_matches('.');
    let mut short: String = sentence.chars().take(90).collect();
    if sentence.chars().count() > 90 {
        short.push('…');
    }
    short
}

pub fn overview(
    listed: Option<(&str, &[Tool])>,
    config: &Config,
    error: Option<&Failure>,
) -> String {
    let mut out = String::from(HEAD);
    match listed {
        Some((server, tools)) => {
            out.push_str(&format!("\ncommands on {server} (from its tools/list):\n"));
            let width = tools.iter().map(|t| t.command().len()).max().unwrap_or(0);
            for tool in tools {
                let mark = if tool.read_only() { "" } else { "  [can write]" };
                out.push_str(&format!(
                    "  {:width$}  {}{mark}\n",
                    tool.command(),
                    first_sentence(&tool.description)
                ));
            }
        }
        None => {
            let why = error.map(|e| e.message.as_str()).unwrap_or("unknown error");
            out.push_str(&format!("\ncould not list the server's commands: {why}\n"));
        }
    }
    out.push_str("\nbuilt in:\n  tools    every command with its parameters, as JSON\n  servers  the configured servers, as JSON (no tokens)\n");
    let names: Vec<&str> = config.sections.iter().map(|s| s.name.as_str()).collect();
    out.push_str(&format!(
        "\nconfig: {PATH} ([name] blocks with url and token; servers: {})\nOutput is always JSON. Commands marked [can write] need --yes when they change something.\n",
        if names.is_empty() { "none".to_string() } else { names.join(", ") }
    ));
    out
}

fn kind(spec: &Value) -> String {
    if let Some(options) = spec.get("enum").and_then(Value::as_array) {
        return options
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|");
    }
    let base = spec.get("type").and_then(Value::as_str).unwrap_or("string");
    match base {
        "array" => format!(
            "{},…",
            spec.get("items")
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("value")
        ),
        other => other.to_string(),
    }
}

pub fn command(tool: &Tool) -> String {
    let mut out = format!("usage: {}\n", usage(tool));
    out.push_str(&format!("tool: {}", tool.name));
    out.push_str(if tool.read_only() {
        " (read-only)\n\n"
    } else {
        " (may change the server: needs --yes, or --dry-run to see the plan)\n\n"
    });
    out.push_str(&tool.description);
    out.push('\n');

    let required = tool.required();
    let properties = tool.properties();
    if !properties.is_empty() {
        out.push_str("\nparameters:\n");
    }
    for (name, spec) in properties {
        let flag = kebab(&name);
        let label = match spec.get("type").and_then(Value::as_str) {
            Some("boolean") => format!("--{flag}"),
            Some("object") if is_secret(&name) => format!("--{flag}-file <.env>"),
            Some("object") => format!("--{flag} KEY=VALUE"),
            _ if is_secret(&name) => format!("--{flag}-file <path|->"),
            _ => format!("--{flag} {}", kind(&spec)),
        };
        let mut notes = Vec::new();
        if required.contains(&name) {
            notes.push("required");
        }
        if tool.positionals().contains(&name) || tool.trailing().as_deref() == Some(name.as_str()) {
            notes.push("or positional");
        }
        if is_secret(&name) {
            notes.push("secret: from a file or stdin, never argv");
        }
        let description = spec
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        };
        out.push_str(&format!("  {label}\n      {description}{notes}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Harness;

    #[test]
    fn shortens_descriptions() {
        assert_eq!(first_sentence("One. Two."), "One");
        assert_eq!(first_sentence(&"x".repeat(100)).chars().count(), 91);
    }

    #[test]
    fn command_help_marks_secrets_and_writes() {
        let h = Harness::new();
        let (env, _) = h.run_raw(&["env", "--help"]);
        assert!(env.contains("--value-file <path|->"));
        assert!(env.contains("needs --yes"));
        let (deploy, _) = h.run_raw(&["deploy", "--help"]);
        assert!(deploy.contains("--env-file <.env>"));
        let (triage, _) = h.run_raw(&["triage", "--help"]);
        assert!(triage.contains("--issue-ids integer,…"));
        let (health, _) = h.run_raw(&["health", "--help"]);
        assert!(health.contains("(read-only)"));
        assert!(!health.contains("parameters:"));
    }

    #[test]
    fn overview_without_a_reason() {
        let text = overview(None, &Config::default(), None);
        assert!(text.contains("unknown error"));
        assert!(text.contains("servers: none"));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind(&serde_json::json!({"type": "array"})), "value,…");
        assert_eq!(kind(&serde_json::json!({})), "string");
    }
}
