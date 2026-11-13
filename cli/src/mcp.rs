use crate::config::Server;
use crate::schema::Tool;
use crate::{Failure, Transport, fail};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Http {
    url: String,
    token: String,
    agent: ureq::Agent,
}

impl Http {
    pub fn new(server: &Server) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        Http {
            url: server.url.clone(),
            token: server.token.clone(),
            agent,
        }
    }
}

impl Transport for Http {
    fn rpc(&self, method: &str, params: Value) -> Result<Value, Failure> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut response = self
            .agent
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(
                "User-Agent",
                concat!("webo-cli/", env!("CARGO_PKG_VERSION")),
            )
            .send(body.to_string())
            .map_err(|e| fail("network", format!("{}: {e}", self.url)))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| fail("network", e.to_string()))?;
        decode(status, &text)
    }
}

pub fn decode(status: u16, body: &str) -> Result<Value, Failure> {
    let payload = event_stream_data(body).unwrap_or(body);
    let parsed: Option<Value> = serde_json::from_str(payload).ok();
    let server_message = parsed.as_ref().and_then(|v| v.get("error")).and_then(|e| {
        e.as_str()
            .map(str::to_string)
            .or_else(|| e.get("message")?.as_str().map(str::to_string))
    });

    if status == 401 || status == 403 {
        return Err(fail(
            "auth",
            server_message
                .unwrap_or_else(|| format!("HTTP {status}: the server refused the token")),
        ));
    }
    if !(200..300).contains(&status) {
        let snippet: String = body.chars().take(200).collect();
        return Err(fail(
            "http",
            server_message.unwrap_or_else(|| format!("HTTP {status}: {snippet}")),
        ));
    }
    let value = parsed.ok_or_else(|| fail("protocol", "the server did not answer JSON"))?;
    if let Some(error) = value.get("error") {
        return Err(
            fail("mcp", server_message.unwrap_or_else(|| error.to_string()))
                .with_detail(json!({"rpc_code": error.get("code")})),
        );
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| fail("protocol", "JSON-RPC answer without a result"))
}

fn event_stream_data(body: &str) -> Option<&str> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .rfind(|data| !data.is_empty())
}

pub fn list_tools(transport: &dyn Transport) -> Result<Vec<Tool>, Failure> {
    let result = transport.rpc("tools/list", json!({}))?;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("protocol", "tools/list answered without tools"))?;
    Ok(tools.iter().filter_map(Tool::from_json).collect())
}

pub fn call_tool(
    transport: &dyn Transport,
    name: &str,
    arguments: Value,
) -> Result<Value, Failure> {
    let result = transport.rpc("tools/call", json!({"name": name, "arguments": arguments}))?;
    let shaped = shape(&result);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = match &shaped {
            Value::Array(lines) => lines
                .iter()
                .filter_map(Value::as_str)
                .find(|l| !l.trim().is_empty())
                .unwrap_or("the tool reported an error")
                .to_string(),
            other => other.to_string(),
        };
        return Err(fail("tool_error", message).with_detail(shaped));
    }
    Ok(shaped)
}

pub fn shape(result: &Value) -> Value {
    if let Some(structured) = result.get("structuredContent") {
        return structured.clone();
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if let Ok(value) = serde_json::from_str::<Value>(&text)
        && (value.is_object() || value.is_array())
    {
        return value;
    }
    let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    json!(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_plain_and_event_stream_answers() {
        assert_eq!(
            decode(200, r#"{"result": {"a": 1}}"#).unwrap(),
            json!({"a": 1})
        );
        let stream = "event: message\ndata: {\"result\": 2}\n\n";
        assert_eq!(decode(200, stream).unwrap(), json!(2));
    }

    #[test]
    fn decodes_failures() {
        let auth = decode(401, r#"{"error":"not authenticated"}"#).unwrap_err();
        assert_eq!(
            (auth.code.as_str(), auth.message.as_str()),
            ("auth", "not authenticated")
        );
        let forbidden = decode(403, "<html>").unwrap_err();
        assert!(forbidden.message.contains("refused the token"));
        let http = decode(502, "bad gateway").unwrap_err();
        assert_eq!(http.message, "HTTP 502: bad gateway");
        let rpc = decode(200, r#"{"error":{"code":-32603,"message":"no project"}}"#).unwrap_err();
        assert_eq!(
            (rpc.code.as_str(), rpc.message.as_str()),
            ("mcp", "no project")
        );
        assert_eq!(rpc.detail, Some(json!({"rpc_code": -32603})));
        let odd = decode(200, r#"{"error":{"code":1}}"#).unwrap_err();
        assert!(odd.message.contains("\"code\":1"));
        assert_eq!(decode(200, "nope").unwrap_err().code, "protocol");
        assert_eq!(decode(200, "{}").unwrap_err().code, "protocol");
    }

    #[test]
    fn shapes_results() {
        let text = json!({"content": [{"type": "text", "text": "a  \nb\n\n"}, {"type": "image"}]});
        assert_eq!(shape(&text), json!(["a", "b"]));
        let data = json!({"content": [{"type": "text", "text": "{\"x\": [1]}"}]});
        assert_eq!(shape(&data), json!({"x": [1]}));
        let scalar = json!({"content": [{"type": "text", "text": "42"}]});
        assert_eq!(shape(&scalar), json!(["42"]));
        assert_eq!(shape(&json!({})), json!([]));
    }

    struct Canned(Result<Value, Failure>);

    impl Transport for Canned {
        fn rpc(&self, _: &str, _: Value) -> Result<Value, Failure> {
            self.0.clone()
        }
    }

    #[test]
    fn tool_errors_carry_the_first_line() {
        let blank = Canned(Ok(json!({"isError": true, "content": []})));
        assert_eq!(
            call_tool(&blank, "t", json!({})).unwrap_err().message,
            "the tool reported an error"
        );
        let structured = Canned(Ok(
            json!({"isError": true, "structuredContent": {"why": "x"}}),
        ));
        assert_eq!(
            call_tool(&structured, "t", json!({})).unwrap_err().message,
            r#"{"why":"x"}"#
        );
    }

    #[test]
    fn list_tools_needs_a_tools_array() {
        let empty = Canned(Ok(json!({})));
        assert_eq!(list_tools(&empty).unwrap_err().code, "protocol");
    }

    #[test]
    fn http_transport_reports_unreachable_servers() {
        let server = Server {
            name: "x".into(),
            url: "http://127.0.0.1:9/mcp".into(),
            token: "t".into(),
        };
        let err = Http::new(&server).rpc("tools/list", json!({})).unwrap_err();
        assert_eq!(err.code, "network");
    }
}
