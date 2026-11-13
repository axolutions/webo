use crate::schema::{Tool, is_secret, kebab, snake};
use crate::{Failure, Io, fail};
use serde_json::{Map, Value, json};

#[derive(Debug, Default, PartialEq)]
pub struct Globals {
    pub server: Option<String>,
    pub yes: bool,
    pub dry_run: bool,
    pub help: bool,
    pub rest: Vec<String>,
}

pub fn split_globals(argv: &[String]) -> Result<Globals, Failure> {
    let mut globals = Globals::default();
    let mut iter = argv.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                globals.rest.push(arg.clone());
                globals.rest.extend(iter.by_ref().cloned());
            }
            "-y" | "--yes" => globals.yes = true,
            "--dry-run" => globals.dry_run = true,
            "-h" | "--help" => globals.help = true,
            "-s" | "--server" => {
                let name = iter
                    .next()
                    .ok_or_else(|| fail("usage", "--server needs a name"))?;
                globals.server = Some(name.clone());
            }
            other => match other.strip_prefix("--server=") {
                Some(name) => globals.server = Some(name.to_string()),
                None => globals.rest.push(arg.clone()),
            },
        }
    }
    if globals.yes && globals.dry_run {
        return Err(fail("usage", "--yes and --dry-run do not go together"));
    }
    if globals.help && globals.rest.is_empty() {
        globals.rest.push("help".into());
    }
    Ok(globals)
}

fn kind(spec: &Value) -> &str {
    spec.get("type").and_then(Value::as_str).unwrap_or("string")
}

pub fn build(tool: &Tool, rest: &[String], io: &Io) -> Result<Map<String, Value>, Failure> {
    let mut out = Map::new();
    let mut positional = Vec::new();
    let mut only_positional = false;
    let mut i = 0;

    while i < rest.len() {
        let arg = &rest[i];
        i += 1;
        if only_positional || !arg.starts_with("--") {
            positional.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }

        let body = &arg[2..];
        let (flag, inline) = match body.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (body, None),
        };
        let unknown = || {
            fail(
                "usage",
                format!(
                    "'{}' has no --{flag}; see 'webo {} --help'",
                    tool.command(),
                    tool.command()
                ),
            )
        };

        if let Some(base) = flag.strip_prefix("no-")
            && let Some(spec) = tool.property(&snake(base))
            && kind(&spec) == "boolean"
            && tool.property(&snake(flag)).is_none()
        {
            out.insert(snake(base), json!(false));
            continue;
        }

        let (name, from_file) = match flag.strip_suffix("-file") {
            Some(base)
                if tool.property(&snake(flag)).is_none()
                    && tool.property(&snake(base)).is_some() =>
            {
                (snake(base), true)
            }
            _ => (snake(flag), false),
        };
        let spec = tool.property(&name).ok_or_else(unknown)?;

        if kind(&spec) == "boolean" && !from_file {
            let value = match inline.as_deref() {
                None | Some("true") => true,
                Some("false") => false,
                Some(other) => {
                    return Err(fail(
                        "usage",
                        format!("--{flag} takes true or false, not '{other}'"),
                    ));
                }
            };
            out.insert(name, json!(value));
            continue;
        }

        let raw = match inline {
            Some(value) => value,
            None => {
                let value = rest
                    .get(i)
                    .ok_or_else(|| fail("usage", format!("--{flag} needs a value")))?;
                i += 1;
                value.clone()
            }
        };

        if from_file {
            let text = read_trimmed(io, &raw)?;
            let value = if kind(&spec) == "object" {
                Value::Object(dotenv(&text))
            } else {
                convert(&name, &spec, &text)?
            };
            out.insert(name, value);
            continue;
        }

        if is_secret(&name) {
            return Err(fail(
                "secret_in_argv",
                format!(
                    "--{flag} would leave the secret in the shell history; use --{flag}-file <path>, or --{flag}-file - to read stdin"
                ),
            ));
        }

        if kind(&spec) == "object" {
            let (key, value) = raw
                .split_once('=')
                .ok_or_else(|| fail("usage", format!("--{flag} takes KEY=VALUE")))?;
            let entry = out.entry(name).or_insert_with(|| json!({}));
            entry[key] = json!(value);
            continue;
        }
        out.insert(name.clone(), convert(&name, &spec, &raw)?);
    }

    let mut slots: Vec<String> = tool
        .positionals()
        .into_iter()
        .filter(|name| !out.contains_key(name))
        .collect();
    if let Some(trailing) = tool.trailing()
        && !out.contains_key(&trailing)
    {
        slots.push(trailing);
    }
    if positional.len() > slots.len() {
        return Err(fail(
            "usage",
            format!(
                "unexpected argument '{}'; usage: {}",
                positional[slots.len()],
                usage(tool)
            ),
        ));
    }
    for (name, raw) in slots.iter().zip(positional) {
        let spec = tool.property(name).unwrap_or(json!({}));
        out.insert(name.clone(), convert(name, &spec, &raw)?);
    }

    let setting = out.get("action").and_then(Value::as_str) == Some("set");
    if setting && tool.property("value").is_some() && !out.contains_key("value") {
        if !io.stdin_piped {
            return Err(fail(
                "usage",
                "the value goes on stdin or in --value-file <path>, never on the command line",
            ));
        }
        let value = read_trimmed(io, "-")?;
        if value.is_empty() {
            return Err(fail(
                "usage",
                "stdin was empty: pipe the value in, or use --value-file <path>",
            ));
        }
        out.insert("value".into(), json!(value));
    }

    for name in tool.required() {
        if !out.contains_key(&name) {
            return Err(fail(
                "usage",
                format!("missing {name}; usage: {}", usage(tool)),
            ));
        }
    }
    Ok(out)
}

fn read_trimmed(io: &Io, path: &str) -> Result<String, Failure> {
    let text = (io.read_input)(path)?;
    let text = text.strip_suffix('\n').unwrap_or(&text);
    Ok(text.strip_suffix('\r').unwrap_or(text).to_string())
}

fn convert(name: &str, spec: &Value, raw: &str) -> Result<Value, Failure> {
    let flag = kebab(name);
    match kind(spec) {
        "integer" => raw.parse::<i64>().map(|n| json!(n)).map_err(|_| {
            fail(
                "usage",
                format!("{flag} must be a whole number, not '{raw}'"),
            )
        }),
        "number" => raw
            .parse::<f64>()
            .map(|n| json!(n))
            .map_err(|_| fail("usage", format!("{flag} must be a number, not '{raw}'"))),
        "boolean" => match raw {
            "true" => Ok(json!(true)),
            "false" => Ok(json!(false)),
            _ => Err(fail(
                "usage",
                format!("{flag} takes true or false, not '{raw}'"),
            )),
        },
        "array" => {
            let items = spec.get("items").cloned().unwrap_or(json!({}));
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|item| convert(name, &items, item))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        _ => {
            if let Some(allowed) = spec.get("enum").and_then(Value::as_array)
                && !allowed.iter().any(|v| v == raw)
            {
                let options: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
                return Err(fail(
                    "usage",
                    format!("{flag} is one of {}, not '{raw}'", options.join("|")),
                ));
            }
            Ok(json!(raw))
        }
    }
}

pub fn dotenv(text: &str) -> Map<String, Value> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let unquoted = ['"', '\'']
                .iter()
                .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
                .unwrap_or(value);
            Some((key.trim().to_string(), json!(unquoted)))
        })
        .collect()
}

pub fn usage(tool: &Tool) -> String {
    let required = tool.required();
    let mut parts = vec![format!("webo {}", tool.command())];
    for name in tool.positionals() {
        if required.contains(&name) {
            parts.push(format!("<{name}>"));
        } else {
            parts.push(format!("[{name}]"));
        }
    }
    if let Some(trailing) = tool.trailing() {
        parts.push(format!("[{trailing}]"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Harness;
    use serde_json::json;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn globals_anywhere_on_the_line() {
        let g = split_globals(&strings(&["status", "-s", "vps", "blogo", "--yes"])).unwrap();
        assert_eq!(g.server.as_deref(), Some("vps"));
        assert!(g.yes);
        assert_eq!(g.rest, ["status", "blogo"]);
        let g = split_globals(&strings(&["--server=vps", "x", "--", "--yes"])).unwrap();
        assert_eq!(g.server.as_deref(), Some("vps"));
        assert!(!g.yes);
        assert_eq!(g.rest, ["x", "--", "--yes"]);
        assert_eq!(split_globals(&strings(&["-h"])).unwrap().rest, ["help"]);
        assert!(split_globals(&strings(&["--server"])).is_err());
        assert!(split_globals(&strings(&["--yes", "--dry-run"])).is_err());
    }

    #[test]
    fn flags_take_every_type() {
        let h = Harness::new();
        h.run(&[
            "rows",
            "blogo",
            "posts",
            "--limit=5",
            "--descending",
            "--order-by",
            "id",
        ]);
        h.run(&["rows", "blogo", "posts", "--no-descending"]);
        h.run(&["triage", "blogo", "3, 4", "resolve", "--yes"]);
        h.run(&["query", "blogo", "--sql", "select 1", "--write=false"]);
        let calls = h.tool_calls();
        assert_eq!(
            calls[0].1["arguments"],
            json!({"slug": "blogo", "table": "posts", "limit": 5, "descending": true, "order_by": "id"})
        );
        assert_eq!(calls[1].1["arguments"]["descending"], false);
        assert_eq!(calls[2].1["arguments"]["issue_ids"], json!([3, 4]));
        assert_eq!(calls[3].1["arguments"]["write"], false);
    }

    #[test]
    fn files_feed_strings_and_objects() {
        let mut h = Harness::new();
        h.files = vec![
            ("q.sql".into(), "select 2\r\n".into()),
            (
                ".env".into(),
                "# c\nexport A=\"1\"\nB='two'\nC=3\nnoise\n".into(),
            ),
        ];
        h.run(&["query", "blogo", "--sql-file", "q.sql"]);
        h.run(&["deploy", "blogo", "--env-file", ".env", "--yes"]);
        let calls = h.tool_calls();
        assert_eq!(calls[0].1["arguments"]["sql"], "select 2");
        assert_eq!(
            calls[1].1["arguments"]["env"],
            json!({"A": "1", "B": "two", "C": "3"})
        );
        let (out, _) = h.run(&["query", "blogo", "--sql-file", "missing"]);
        assert_eq!(out["error"]["code"], "input");
    }

    #[test]
    fn object_flags_take_key_value_pairs() {
        let tool = crate::schema::Tool::from_json(&json!({
            "name": "tag",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"properties": {"labels": {"type": "object"}, "ratio": {"type": "number"}}}
        }))
        .unwrap();
        let args = |line: &[&str]| -> Result<Map<String, Value>, Failure> {
            let var = |_: &str| None;
            let read = |_: &str| Err(fail("input", "none"));
            let connect =
                |_: &crate::config::Server| -> Box<dyn crate::Transport> { unreachable!() };
            let io = Io {
                config: None,
                var: &var,
                read_input: &read,
                stdin_piped: false,
                connect: &connect,
            };
            build(&tool, &strings(line), &io)
        };
        assert_eq!(
            args(&["--labels", "a=1", "--labels=b=2", "--ratio", "0.5"]).unwrap(),
            json!({"labels": {"a": "1", "b": "2"}, "ratio": 0.5})
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(args(&["--labels", "nope"]).unwrap_err().code, "usage");
        assert_eq!(args(&["--ratio", "x"]).unwrap_err().code, "usage");
        assert_eq!(
            args(&["--ratio"]).unwrap_err().message,
            "--ratio needs a value"
        );
        assert_eq!(args(&["extra"]).unwrap_err().code, "usage");
    }

    #[test]
    fn bad_input_is_a_usage_error() {
        let h = Harness::new();
        for line in [
            vec!["rows", "blogo", "posts", "--limit", "x"],
            vec!["rows", "blogo", "posts", "--descending=maybe"],
            vec!["logs", "blogo", "--window", "1y"],
            vec!["status", "blogo", "--nope"],
            vec!["status"],
            vec!["status", "a", "b"],
            vec!["env", "blogo", "set", "K"],
        ] {
            let (out, code) = h.run(&line);
            assert_eq!(code, 2, "{line:?} -> {out}");
        }
        let (out, _) = h.run(&["status", "--", "--weird"]);
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn stdin_value_is_read_once_and_trimmed() {
        let mut h = Harness::new();
        h.stdin = Some("abc\n".into());
        h.run(&[
            "env",
            "blogo",
            "set",
            "--key",
            "K",
            "--value-file",
            "-",
            "--yes",
        ]);
        assert_eq!(h.tool_calls()[0].1["arguments"]["value"], "abc");
    }

    #[test]
    fn empty_stdin_is_refused() {
        let mut h = Harness::new();
        h.stdin = Some("\n".into());
        let (out, code) = h.run(&["env", "blogo", "set", "K", "--yes"]);
        assert_eq!(code, 2);
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stdin was empty")
        );
    }

    #[test]
    fn usage_lines() {
        let h = Harness::new();
        let (text, _) = h.run_raw(&["env", "-h"]);
        assert!(text.contains("webo env <slug> [action] [key]"), "{text}");
    }

    #[test]
    fn dotenv_handles_quotes() {
        let env = dotenv("A='x'\nB=\"y\"\nC=z");
        assert_eq!(Value::Object(env), json!({"A": "x", "B": "y", "C": "z"}));
    }
}
