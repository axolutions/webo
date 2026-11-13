use serde_json::{Map, Value, json};

pub const MASK: &str = "••••";

const ALIASES: &[(&str, &str)] = &[
    ("list_projects", "projects"),
    ("project_status", "status"),
    ("project_metrics", "metrics"),
    ("project_env", "env"),
    ("server_health", "health"),
    ("server_processes", "processes"),
    ("search_logs", "logs"),
    ("tail_logs", "tail"),
    ("list_errors", "errors"),
    ("error_detail", "error"),
    ("triage_errors", "triage"),
    ("db_info", "db"),
    ("db_rows", "rows"),
    ("db_query", "query"),
    ("db_backup", "backup"),
    ("create_project", "create"),
    ("deploy_project", "deploy"),
    ("connect_domain", "domain"),
    ("delete_project", "delete"),
];

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub annotations: Value,
}

pub fn kebab(name: &str) -> String {
    name.replace('_', "-")
}

pub fn snake(flag: &str) -> String {
    flag.replace('-', "_")
}

pub fn is_secret(property: &str) -> bool {
    matches!(property, "value" | "env" | "database_url")
        || ["token", "password", "secret"]
            .iter()
            .any(|word| property.contains(word))
}

pub fn find<'a>(tools: &'a [Tool], command: &str) -> Option<&'a Tool> {
    tools.iter().find(|t| t.command() == command).or_else(|| {
        tools
            .iter()
            .find(|t| t.name == command || kebab(&t.name) == command)
    })
}

impl Tool {
    pub fn from_json(value: &Value) -> Option<Tool> {
        Some(Tool {
            name: value.get("name")?.as_str()?.to_string(),
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            schema: value.get("inputSchema").cloned().unwrap_or(json!({})),
            annotations: value.get("annotations").cloned().unwrap_or(json!({})),
        })
    }

    pub fn command(&self) -> String {
        ALIASES
            .iter()
            .find(|(tool, _)| *tool == self.name)
            .map(|(_, alias)| alias.to_string())
            .unwrap_or_else(|| kebab(&self.name))
    }

    pub fn properties(&self) -> Vec<(String, Value)> {
        self.schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    pub fn property(&self, name: &str) -> Option<Value> {
        self.schema.get("properties")?.get(name).cloned()
    }

    pub fn required(&self) -> Vec<String> {
        self.schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| {
                r.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn read_only(&self) -> bool {
        self.annotations
            .get("readOnlyHint")
            .and_then(Value::as_bool)
            == Some(true)
    }

    pub fn positionals(&self) -> Vec<String> {
        let mut names = self.required();
        let action_is_optional = !names.iter().any(|n| n == "action");
        if action_is_optional
            && self
                .property("action")
                .is_some_and(|p| p.get("enum").is_some())
        {
            names.push("action".into());
        }
        names
    }

    pub fn trailing(&self) -> Option<String> {
        let required = self.required();
        self.properties()
            .into_iter()
            .find(|(name, spec)| {
                name != "action"
                    && !required.contains(name)
                    && !is_secret(name)
                    && spec.get("type").and_then(Value::as_str) == Some("string")
                    && spec.get("enum").is_none()
            })
            .map(|(name, _)| name)
    }

    pub fn is_write(&self, arguments: &Map<String, Value>) -> bool {
        if self.read_only() {
            return false;
        }
        if let Some(action) = self.property("action") {
            let lists = action
                .get("enum")
                .and_then(Value::as_array)
                .is_some_and(|e| e.iter().any(|v| v == "list"));
            let defaults_to_list = action
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| d.contains("Defaults to list"));
            let chosen = arguments.get("action").and_then(Value::as_str);
            if lists && (chosen == Some("list") || chosen.is_none() && defaults_to_list) {
                return false;
            }
        }
        if self
            .property("write")
            .is_some_and(|p| p.get("type").and_then(Value::as_str) == Some("boolean"))
        {
            return arguments.get("write").and_then(Value::as_bool) == Some(true);
        }
        true
    }

    pub fn auto_confirm(&self, arguments: &mut Map<String, Value>) {
        let boolean = self
            .property("confirm")
            .is_some_and(|p| p.get("type").and_then(Value::as_str) == Some("boolean"));
        if boolean && !arguments.contains_key("confirm") {
            arguments.insert("confirm".into(), json!(true));
        }
    }

    pub fn mask(&self, arguments: &Map<String, Value>) -> Value {
        let masked: Map<String, Value> = arguments
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::Object(inner) if is_secret(key) => {
                        Value::Object(inner.keys().map(|k| (k.clone(), json!(MASK))).collect())
                    }
                    _ if is_secret(key) => json!(MASK),
                    other => other.clone(),
                };
                (key.clone(), value)
            })
            .collect();
        Value::Object(masked)
    }

    pub fn describe(&self) -> Value {
        json!({
            "command": self.command(),
            "tool": self.name,
            "read_only": self.read_only(),
            "description": self.description,
            "positionals": self.positionals(),
            "trailing": self.trailing(),
            "parameters": self.schema.get("properties").cloned().unwrap_or(json!({})),
            "required": self.required(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TOOLS;

    pub fn tools() -> Vec<Tool> {
        let list: Value = serde_json::from_str(TOOLS).unwrap();
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Tool::from_json)
            .collect()
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn every_tool_has_a_command() {
        let tools = tools();
        assert_eq!(find(&tools, "env").unwrap().name, "project_env");
        assert_eq!(find(&tools, "project-env").unwrap().name, "project_env");
        assert_eq!(find(&tools, "project_env").unwrap().name, "project_env");
        let fresh = Tool::from_json(&json!({"name": "new_thing"})).unwrap();
        assert_eq!(fresh.command(), "new-thing");
        assert!(fresh.properties().is_empty() && fresh.required().is_empty());
        assert!(Tool::from_json(&json!({})).is_none());
    }

    #[test]
    fn positionals_and_trailing() {
        let tools = tools();
        let env = find(&tools, "env").unwrap();
        assert_eq!(env.positionals(), ["slug", "action"]);
        assert_eq!(env.trailing().as_deref(), Some("key"));
        let triage = find(&tools, "triage").unwrap();
        assert_eq!(triage.positionals(), ["slug", "issue_ids", "action"]);
        assert_eq!(
            find(&tools, "logs").unwrap().trailing().as_deref(),
            Some("query")
        );
        assert_eq!(find(&tools, "deploy").unwrap().trailing(), None);
    }

    #[test]
    fn decides_what_writes() {
        let tools = tools();
        let env = find(&tools, "env").unwrap();
        assert!(!env.is_write(&args(json!({}))));
        assert!(!env.is_write(&args(json!({"action": "list"}))));
        assert!(env.is_write(&args(json!({"action": "set"}))));
        let query = find(&tools, "query").unwrap();
        assert!(!query.is_write(&args(json!({"sql": "select 1"}))));
        assert!(query.is_write(&args(json!({"write": true}))));
        assert!(!find(&tools, "status").unwrap().is_write(&Map::new()));
        assert!(find(&tools, "domain").unwrap().is_write(&Map::new()));
        assert!(find(&tools, "delete").unwrap().is_write(&Map::new()));
    }

    #[test]
    fn masks_secrets() {
        let tools = tools();
        let deploy = find(&tools, "deploy").unwrap();
        let masked = deploy.mask(&args(
            json!({"slug": "a", "env": {"K": "v"}, "database_url": "pg://x"}),
        ));
        assert_eq!(
            masked,
            json!({"slug": "a", "env": {"K": MASK}, "database_url": MASK})
        );
        assert!(is_secret("api_token") && is_secret("password") && !is_secret("key"));
    }

    #[test]
    fn auto_confirm_only_for_boolean_confirm() {
        let tools = tools();
        let mut backup = Map::new();
        find(&tools, "backup").unwrap().auto_confirm(&mut backup);
        assert_eq!(backup.get("confirm"), Some(&json!(true)));
        let mut delete = Map::new();
        find(&tools, "delete").unwrap().auto_confirm(&mut delete);
        assert!(delete.is_empty());
    }
}
