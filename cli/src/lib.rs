pub mod args;
pub mod config;
pub mod help;
pub mod mcp;
pub mod schema;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub detail: Option<Value>,
}

pub fn fail(code: &str, message: impl Into<String>) -> Failure {
    Failure {
        code: code.into(),
        message: message.into(),
        detail: None,
    }
}

impl Failure {
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn to_json(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(detail) = &self.detail {
            error["detail"] = detail.clone();
        }
        json!({"ok": false, "error": error})
    }

    fn exit_code(&self) -> u8 {
        match self.code.as_str() {
            "usage" | "confirmation_required" | "secret_in_argv" => 2,
            _ => 1,
        }
    }
}

pub trait Transport {
    fn rpc(&self, method: &str, params: Value) -> Result<Value, Failure>;
}

pub struct Io<'a> {
    pub config: Option<String>,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub read_input: &'a dyn Fn(&str) -> Result<String, Failure>,
    pub stdin_piped: bool,
    pub connect: &'a dyn Fn(&config::Server) -> Box<dyn Transport + 'a>,
}

#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub stdout: String,
    pub code: u8,
}

enum Reply {
    Json(Value),
    Text(String),
}

pub fn run(argv: &[String], io: &Io) -> Outcome {
    match execute(argv, io) {
        Ok(Reply::Json(value)) => Outcome {
            stdout: pretty(&value),
            code: 0,
        },
        Ok(Reply::Text(text)) => Outcome {
            stdout: text,
            code: 0,
        },
        Err(failure) => Outcome {
            stdout: pretty(&failure.to_json()),
            code: failure.exit_code(),
        },
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn execute(argv: &[String], io: &Io) -> Result<Reply, Failure> {
    let globals = args::split_globals(argv)?;
    let cfg = config::parse(io.config.as_deref().unwrap_or(""));
    let command = globals.rest.first().cloned();

    if command.as_deref() == Some("servers") {
        return Ok(Reply::Json(config::describe(&cfg, io.var)));
    }

    let wants_overview =
        command.is_none() || command.as_deref() == Some("help") && globals.rest.len() == 1;
    let server = match config::resolve(&cfg, globals.server.as_deref(), io.var) {
        Ok(server) => server,
        Err(failure) if wants_overview => {
            return Ok(Reply::Text(help::overview(None, &cfg, Some(&failure))));
        }
        Err(failure) => return Err(failure),
    };
    let transport = (io.connect)(&server);
    let tools = match mcp::list_tools(transport.as_ref()) {
        Ok(tools) => tools,
        Err(failure) if wants_overview => {
            return Ok(Reply::Text(help::overview(None, &cfg, Some(&failure))));
        }
        Err(failure) => return Err(failure),
    };

    let Some(mut name) = command else {
        return Ok(Reply::Text(help::overview(
            Some((&server.name, &tools)),
            &cfg,
            None,
        )));
    };
    let mut rest = globals.rest[1..].to_vec();
    let mut wants_help = globals.help;
    if name == "help" {
        let Some(target) = rest.first().cloned() else {
            return Ok(Reply::Text(help::overview(
                Some((&server.name, &tools)),
                &cfg,
                None,
            )));
        };
        name = target;
        rest.clear();
        wants_help = true;
    }

    if name == "tools" {
        return Ok(Reply::Json(json!({
            "ok": true,
            "server": server.name,
            "tools": tools.iter().map(schema::Tool::describe).collect::<Vec<_>>(),
        })));
    }

    let tool = schema::find(&tools, &name).ok_or_else(|| {
        fail(
            "usage",
            format!("unknown command '{name}'; run 'webo --help' for the list"),
        )
    })?;
    if wants_help {
        return Ok(Reply::Text(help::command(tool)));
    }

    let mut arguments = args::build(tool, &rest, io)?;
    let write = tool.is_write(&arguments);
    if write && globals.yes {
        tool.auto_confirm(&mut arguments);
    }
    let plan = json!({
        "server": server.name,
        "url": server.url,
        "tool": tool.name,
        "write": write,
        "arguments": tool.mask(&arguments),
    });
    if globals.dry_run {
        return Ok(Reply::Json(
            json!({"ok": true, "dry_run": true, "plan": plan}),
        ));
    }
    if write && !globals.yes {
        return Err(fail(
            "confirmation_required",
            format!(
                "'{}' changes the server; rerun with --yes to run it, or --dry-run to only print the plan",
                tool.command()
            ),
        )
        .with_detail(json!({"plan": plan})));
    }

    let result = mcp::call_tool(transport.as_ref(), &tool.name, Value::Object(arguments))?;
    Ok(Reply::Json(json!({
        "ok": true,
        "server": server.name,
        "tool": tool.name,
        "result": result,
    })))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub const TOOLS: &str = include_str!("../tests/tools.json");

    pub const CONFIG: &str = "default = homelab\n\n[homelab]\nurl = https://home.example/mcp\ntoken = home-token\n\n[vps]\nurl = https://vps.example/mcp\ntoken = vps-token\n";

    pub type Calls = Rc<RefCell<Vec<(String, String, Value)>>>;
    pub type Reply = Rc<dyn Fn(&str, &Value) -> Result<Value, Failure>>;

    pub struct Fake {
        pub server: String,
        pub list_fails: bool,
        pub calls: Calls,
        pub reply: Reply,
    }

    impl Transport for Fake {
        fn rpc(&self, method: &str, params: Value) -> Result<Value, Failure> {
            self.calls
                .borrow_mut()
                .push((self.server.clone(), method.to_string(), params.clone()));
            if method == "tools/list" {
                if self.list_fails {
                    return Err(fail("network", "connection refused"));
                }
                let list: Value = serde_json::from_str(TOOLS).unwrap();
                return Ok(list["result"].clone());
            }
            (self.reply)(method, &params)
        }
    }

    pub fn text_result(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    pub struct Harness {
        pub calls: Calls,
        pub config: Option<String>,
        pub vars: Vec<(String, String)>,
        pub stdin: Option<String>,
        pub files: Vec<(String, String)>,
        pub list_fails: bool,
        pub reply: Reply,
    }

    impl Harness {
        pub fn new() -> Self {
            Harness {
                calls: Rc::new(RefCell::new(Vec::new())),
                config: Some(CONFIG.to_string()),
                vars: Vec::new(),
                stdin: None,
                files: Vec::new(),
                list_fails: false,
                reply: Rc::new(|_, params| {
                    Ok(text_result(&format!(
                        "called {}",
                        params["name"].as_str().unwrap_or("")
                    )))
                }),
            }
        }

        pub fn run(&self, line: &[&str]) -> (Value, u8) {
            let (stdout, code) = self.run_raw(line);
            let value = serde_json::from_str(&stdout).unwrap_or(Value::String(stdout));
            (value, code)
        }

        pub fn run_raw(&self, line: &[&str]) -> (String, u8) {
            let argv: Vec<String> = line.iter().map(|s| s.to_string()).collect();
            let vars = self.vars.clone();
            let var = move |key: &str| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            let stdin = self.stdin.clone();
            let files = self.files.clone();
            let read_input = move |path: &str| -> Result<String, Failure> {
                if path == "-" {
                    return stdin.clone().ok_or_else(|| fail("input", "no stdin"));
                }
                files
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, c)| c.clone())
                    .ok_or_else(|| fail("input", format!("cannot read {path}")))
            };
            let calls = self.calls.clone();
            let reply = self.reply.clone();
            let list_fails = self.list_fails;
            let connect = move |server: &config::Server| -> Box<dyn Transport> {
                Box::new(Fake {
                    server: server.name.clone(),
                    list_fails,
                    calls: calls.clone(),
                    reply: reply.clone(),
                })
            };
            let io = Io {
                config: self.config.clone(),
                var: &var,
                read_input: &read_input,
                stdin_piped: self.stdin.is_some(),
                connect: &connect,
            };
            let outcome = run(&argv, &io);
            (outcome.stdout, outcome.code)
        }

        pub fn tool_calls(&self) -> Vec<(String, Value)> {
            self.calls
                .borrow()
                .iter()
                .filter(|(_, method, _)| method == "tools/call")
                .map(|(server, _, params)| (server.clone(), params.clone()))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::rc::Rc;

    #[test]
    fn projects_calls_list_projects_on_the_default_server() {
        let h = Harness::new();
        let (out, code) = h.run(&["projects"]);
        assert_eq!(code, 0);
        assert_eq!(out["ok"], true);
        assert_eq!(out["server"], "homelab");
        assert_eq!(out["tool"], "list_projects");
        assert_eq!(out["result"], json!(["called list_projects"]));
        assert_eq!(
            h.tool_calls()[0].1,
            json!({"name": "list_projects", "arguments": {}})
        );
    }

    #[test]
    fn server_flag_picks_another_block() {
        let h = Harness::new();
        let (out, _) = h.run(&["--server", "vps", "projects"]);
        assert_eq!(out["server"], "vps");
        assert_eq!(h.tool_calls()[0].0, "vps");
    }

    #[test]
    fn status_maps_the_slug_positionally() {
        let h = Harness::new();
        h.run(&["status", "blogo"]);
        assert_eq!(h.tool_calls()[0].1["arguments"], json!({"slug": "blogo"}));
    }

    #[test]
    fn env_list_is_a_read() {
        let h = Harness::new();
        let (out, code) = h.run(&["env", "blogo"]);
        assert_eq!(code, 0);
        assert_eq!(out["tool"], "project_env");
    }

    #[test]
    fn env_set_without_yes_shows_the_plan_and_sends_nothing() {
        let mut h = Harness::new();
        h.stdin = Some("s3cret\n".into());
        let (out, code) = h.run(&["env", "blogo", "set", "API_KEY"]);
        assert_eq!(code, 2);
        assert_eq!(out["error"]["code"], "confirmation_required");
        let plan = &out["error"]["detail"]["plan"];
        assert_eq!(plan["arguments"]["value"], "••••");
        assert_eq!(plan["arguments"]["key"], "API_KEY");
        assert!(!out.to_string().contains("s3cret"));
        assert!(h.tool_calls().is_empty());
    }

    #[test]
    fn env_set_with_yes_reads_stdin() {
        let mut h = Harness::new();
        h.stdin = Some("s3cret\n".into());
        let (out, code) = h.run(&["env", "blogo", "set", "API_KEY", "--yes"]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            h.tool_calls()[0].1["arguments"],
            json!({"slug": "blogo", "action": "set", "key": "API_KEY", "value": "s3cret"})
        );
    }

    #[test]
    fn dry_run_prints_the_plan_without_calling() {
        let mut h = Harness::new();
        h.files = vec![("v.txt".into(), "abc\n".into())];
        let (out, code) = h.run(&[
            "--dry-run",
            "env",
            "blogo",
            "set",
            "--key",
            "K",
            "--value-file",
            "v.txt",
        ]);
        assert_eq!(code, 0);
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["plan"]["write"], true);
        assert_eq!(out["plan"]["url"], "https://home.example/mcp");
        assert_eq!(out["plan"]["arguments"]["value"], "••••");
        assert!(h.tool_calls().is_empty());
    }

    #[test]
    fn secrets_on_the_command_line_are_refused() {
        let h = Harness::new();
        let (out, code) = h.run(&["env", "blogo", "set", "K", "--value", "abc"]);
        assert_eq!(code, 2);
        assert_eq!(out["error"]["code"], "secret_in_argv");
        assert!(!out.to_string().contains("abc"));
    }

    #[test]
    fn yes_fills_a_boolean_confirm() {
        let h = Harness::new();
        h.run(&["backup", "blogo", "restore", "dump.sql", "-y"]);
        assert_eq!(
            h.tool_calls()[0].1["arguments"],
            json!({"slug": "blogo", "action": "restore", "file": "dump.sql", "confirm": true})
        );
    }

    #[test]
    fn delete_needs_the_slug_typed_again() {
        let h = Harness::new();
        let (out, code) = h.run(&["delete", "blogo", "--yes"]);
        assert_eq!(code, 2);
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap()
                .contains("confirm")
        );
        let (_, code) = h.run(&["delete", "blogo", "blogo", "--yes"]);
        assert_eq!(code, 0);
        assert_eq!(
            h.tool_calls()[0].1["arguments"],
            json!({"slug": "blogo", "confirm": "blogo"})
        );
    }

    #[test]
    fn tool_errors_become_failures() {
        let mut h = Harness::new();
        h.reply = Rc::new(|_, _| {
            Ok(
                json!({"isError": true, "content": [{"type": "text", "text": "no project named x\nmore"}]}),
            )
        });
        let (out, code) = h.run(&["status", "x"]);
        assert_eq!(code, 1);
        assert_eq!(out["error"]["code"], "tool_error");
        assert_eq!(out["error"]["message"], "no project named x");
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        let h = Harness::new();
        let (out, code) = h.run(&["frobnicate"]);
        assert_eq!(code, 2);
        assert_eq!(out["error"]["code"], "usage");
    }

    #[test]
    fn help_lists_commands_from_the_server() {
        let h = Harness::new();
        let (text, code) = h.run_raw(&["--help"]);
        assert_eq!(code, 0);
        assert!(text.contains("projects"));
        assert!(text.contains("deploy"));
        assert!(text.contains("commands on homelab"));
        let (same, _) = h.run_raw(&["help"]);
        assert_eq!(text, same);
        let (bare, _) = h.run_raw(&[]);
        assert_eq!(text, bare);
    }

    #[test]
    fn command_help_shows_parameters() {
        let h = Harness::new();
        let (text, _) = h.run_raw(&["logs", "--help"]);
        assert!(text.contains("webo logs <slug> [query]"));
        assert!(text.contains("--window now|24h|7d"));
        let (same, _) = h.run_raw(&["help", "logs"]);
        assert_eq!(text, same);
    }

    #[test]
    fn help_still_works_when_the_server_is_unreachable() {
        let mut h = Harness::new();
        h.config = None;
        let (text, code) = h.run_raw(&["--help"]);
        assert_eq!(code, 0);
        assert!(text.contains("usage: webo"));
        assert!(text.contains("no server configured"));
    }

    #[test]
    fn commands_fail_without_config() {
        let mut h = Harness::new();
        h.config = None;
        let (out, code) = h.run(&["projects"]);
        assert_eq!(code, 1);
        assert_eq!(out["error"]["code"], "config");
    }

    #[test]
    fn tools_lists_every_tool_as_json() {
        let h = Harness::new();
        let (out, _) = h.run(&["tools"]);
        let tools = out["tools"].as_array().unwrap();
        assert!(
            tools
                .iter()
                .any(|t| t["command"] == "env" && t["tool"] == "project_env")
        );
    }

    #[test]
    fn servers_needs_no_network() {
        let h = Harness::new();
        let (out, _) = h.run(&["servers"]);
        assert_eq!(out["default"], "homelab");
        assert_eq!(out["servers"][1]["name"], "vps");
        assert!(!out.to_string().contains("vps-token"));
        assert!(h.calls.borrow().is_empty());
    }

    #[test]
    fn transport_failures_are_reported() {
        let mut h = Harness::new();
        h.reply = Rc::new(|_, _| Err(fail("network", "down")));
        let (out, code) = h.run(&["status", "x"]);
        assert_eq!(code, 1);
        assert_eq!(out["error"]["code"], "network");
        h.list_fails = true;
        let (out, code) = h.run(&["status", "x"]);
        assert_eq!(code, 1);
        assert_eq!(out["error"]["message"], "connection refused");
        let (text, code) = h.run_raw(&["--help"]);
        assert_eq!(code, 0);
        assert!(text.contains("connection refused"));
    }

    #[test]
    fn structured_content_is_returned_as_is() {
        let mut h = Harness::new();
        h.reply = Rc::new(|_, _| Ok(json!({"structuredContent": {"a": 1}})));
        let (out, _) = h.run(&["status", "x"]);
        assert_eq!(out["result"], json!({"a": 1}));
    }
}
