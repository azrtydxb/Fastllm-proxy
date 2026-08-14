//! The MCP gateway: one endpoint in front of every tool server.
//!
//! # What this is for
//!
//! The same argument as the model gateway, one layer up. A team that runs
//! four MCP servers otherwise gives every agent four addresses, four
//! credentials and four sets of trust decisions, and has no single place to
//! answer "which of our keys can reach the one that writes to production".
//! Here a server is a row, a grant is `mcp:invoke` on `mcp/<name>`, and the
//! answer is the same query it already is for models.
//!
//! # Tools are namespaced, and that is not cosmetic
//!
//! Two servers may both expose `search`. A tool name is what the model emits
//! in a tool call, so a collision is not a listing problem — it is the gateway
//! being unable to tell which server the model meant, after the fact, with no
//! way to ask. Every tool is therefore `<server>__<tool>` on the way out and
//! split on the way back in. MCP's own spec landed on the same conclusion
//! (SEP-986); the separator here is `__` because tool names are frequently
//! constrained to `[a-zA-Z0-9_-]` and a dot or a slash would be rewritten by
//! something downstream.
//!
//! # Why this may do I/O when the model path may not
//!
//! `tests/no_io_on_hot_path.rs` guards the *forwarding* path: authorisation,
//! rate limits and routing all resolve against an in-memory snapshot because
//! they happen on every token of every request. An MCP tool call is not that.
//! It is a request whose entire purpose is to reach another server, exactly
//! like proxying a completion — the I/O *is* the work. What still holds is
//! that deciding **whether** the caller may make it costs one set lookup.

use serde::{Deserialize, Serialize};

use crate::snapshot::{McpServerDef, Principal, Snapshot};

/// The separator between a server name and a tool name.
///
/// See the module doc: `.`/`/` get rewritten by clients and providers that
/// constrain tool names, and `_` alone collides with the many tool names that
/// already contain one.
pub const NAMESPACE_SEP: &str = "__";

/// A tool as the gateway presents it: the server's own description, with the
/// name namespaced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        rename = "inputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub input_schema: Option<serde_json::Value>,
    /// Which server this came from, so a caller does not have to parse the
    /// name to find out.
    pub server: String,
}

/// What a caller may reach, and why they can see it.
#[derive(Debug, Clone, Serialize)]
pub struct ServerView {
    pub name: String,
    pub description: String,
    pub transport: String,
    /// Never the credential, only whether one is set — the same rule the
    /// model API follows.
    pub credential_set: bool,
}

/// The servers this principal may invoke.
///
/// An unauthenticated deployment (`open`) sees all of them: there are no
/// grants to check against, which is the same convention `models_response`
/// already uses.
pub fn visible_servers(snap: &Snapshot, principal: Option<&Principal>) -> Vec<ServerView> {
    let mut out: Vec<ServerView> = snap
        .mcp_servers
        .values()
        .filter(|s| may_invoke(principal, &s.name, snap.open))
        .map(|s| ServerView {
            name: s.name.clone(),
            description: s.description.clone(),
            transport: s.transport.clone(),
            credential_set: s.api_key.is_some(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One set lookup, and the reason this is affordable per request.
pub fn may_invoke(principal: Option<&Principal>, server: &str, open: bool) -> bool {
    match principal {
        Some(p) => p.allow_all_mcp || p.allowed_mcp.contains(server),
        // No key required at all: there is nothing to check.
        None => open,
    }
}

/// Split a namespaced tool name back into its server and its own name.
///
/// Returns `None` for an un-namespaced name rather than guessing a server:
/// picking one would mean a tool call landing somewhere the caller did not
/// name, which is the failure this namespacing exists to prevent.
pub fn split_tool(qualified: &str) -> Option<(&str, &str)> {
    qualified.split_once(NAMESPACE_SEP)
}

/// The namespaced name for a tool on a server.
pub fn qualify(server: &str, tool: &str) -> String {
    format!("{server}{NAMESPACE_SEP}{tool}")
}

/// The headers to present to a server, built the same way a backend's are.
pub fn auth_headers(server: &McpServerDef) -> Vec<(String, String)> {
    let mut headers = vec![(
        "accept".to_string(),
        "application/json, text/event-stream".to_string(),
    )];
    if let Some(key) = &server.api_key {
        let value = match server.auth_scheme.as_deref() {
            Some(scheme) if !scheme.is_empty() => format!("{scheme} {key}"),
            // Raw, which is what several MCP hosts expect.
            _ => key.clone(),
        };
        headers.push((server.auth_header.clone(), value));
    }
    headers
}

/// A JSON-RPC request body, which is what MCP speaks over both transports.
pub fn rpc(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Pull the tool list out of a server's `tools/list` result, namespacing as it
/// goes.
///
/// A server that answers with something unexpected contributes nothing rather
/// than failing the whole aggregate: one broken server in a list of four
/// should not take the other three down with it, and the caller can see which
/// ones answered from the `server` field on what they did get.
pub fn tools_from_result(server: &str, body: &serde_json::Value) -> Vec<Tool> {
    let Some(list) = body
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            Some(Tool {
                name: qualify(server, name),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
                input_schema: t.get("inputSchema").cloned(),
                server: server.to_string(),
            })
        })
        .collect()
}

/// The tools a model can be handed, in OpenAI's `tools` shape.
///
/// This is the bridge that makes the gateway worth having: an MCP server's
/// catalogue becomes something any OpenAI-compatible model can be given
/// without the caller translating anything.
pub fn as_openai_tools(tools: &[Tool]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description.clone().unwrap_or_default(),
                    "parameters": t.input_schema.clone().unwrap_or(serde_json::json!({
                        "type": "object", "properties": {}
                    })),
                }
            })
        })
        .collect()
}

/// Errors this gateway returns, each mapped to the status it deserves.
#[derive(Debug, PartialEq, Eq)]
pub enum McpError {
    /// The tool name carried no server namespace.
    Unqualified,
    /// No such server, or the caller may not reach it — deliberately the same
    /// answer, so an unauthorised caller cannot enumerate what exists.
    NotFoundOrForbidden(String),
}

impl McpError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Unqualified => 400,
            // 404, not 403: a caller without a grant learns nothing about
            // whether the server exists.
            Self::NotFoundOrForbidden(_) => 404,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Unqualified => format!(
                "tool name must be namespaced as <server>{NAMESPACE_SEP}<tool> — call \
                 GET /v1/mcp/servers to see which servers this key may use"
            ),
            Self::NotFoundOrForbidden(s) => format!("no MCP server {s:?} available to this key"),
        }
    }
}

/// Resolve a namespaced tool to the server that should receive it.
pub fn route<'a>(
    snap: &'a Snapshot,
    principal: Option<&Principal>,
    qualified: &str,
) -> Result<(&'a McpServerDef, String), McpError> {
    let Some((server, tool)) = split_tool(qualified) else {
        return Err(McpError::Unqualified);
    };
    let Some(def) = snap.mcp_servers.get(server) else {
        return Err(McpError::NotFoundOrForbidden(server.to_string()));
    };
    if !may_invoke(principal, server, snap.open) {
        return Err(McpError::NotFoundOrForbidden(server.to_string()));
    }
    Ok((def, tool.to_string()))
}

/// Everything a caller may reach, for the aggregate `tools/list`.
pub fn invocable_servers<'a>(
    snap: &'a Snapshot,
    principal: Option<&Principal>,
) -> Vec<&'a McpServerDef> {
    let mut out: Vec<&McpServerDef> = snap
        .mcp_servers
        .values()
        .filter(|def| may_invoke(principal, &def.name, snap.open))
        .collect();
    // Stable order, so an aggregate listing does not reshuffle between calls
    // for reasons a caller cannot see.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn principal(mcp: &[&str], all: bool) -> Principal {
        Principal {
            id: 1,
            name: "p".into(),
            allowed_models: HashSet::new(),
            allow_all: false,
            allowed_mcp: mcp.iter().map(|s| s.to_string()).collect(),
            allow_all_mcp: all,
            roles: HashSet::new(),
            limits: None,
            budget: None,
        }
    }

    fn snap(servers: &[&str]) -> Snapshot {
        let mut s = Snapshot::default();
        s.mcp_servers = servers
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    McpServerDef {
                        name: n.to_string(),
                        url: format!("https://{n}.example/mcp"),
                        transport: "http".into(),
                        description: String::new(),
                        auth_header: "authorization".into(),
                        auth_scheme: Some("Bearer".into()),
                        api_key: Some("sk-x".into()),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        s
    }

    /// Two servers exposing `search` is the ordinary case, not the exotic one.
    #[test]
    fn a_tool_name_carries_the_server_it_came_from() {
        let body = serde_json::json!({
            "result": {"tools": [{"name": "search", "description": "d",
                                  "inputSchema": {"type": "object"}}]}
        });
        let a = tools_from_result("github", &body);
        let b = tools_from_result("jira", &body);
        assert_eq!(a[0].name, "github__search");
        assert_eq!(b[0].name, "jira__search");
        assert_ne!(a[0].name, b[0].name, "a collision must not be possible");
        assert_eq!(split_tool(&a[0].name), Some(("github", "search")));
    }

    /// An un-namespaced name is refused rather than guessed. Guessing means a
    /// tool call landing on a server the caller did not name.
    #[test]
    fn an_unqualified_tool_name_is_refused_not_guessed() {
        let s = snap(&["github"]);
        let p = principal(&[], true);
        assert_eq!(
            route(&s, Some(&p), "search").unwrap_err(),
            McpError::Unqualified
        );
        assert_eq!(McpError::Unqualified.status(), 400);
    }

    /// A caller without the grant learns nothing about what exists.
    #[test]
    fn a_server_the_caller_cannot_reach_is_indistinguishable_from_one_that_does_not_exist() {
        let s = snap(&["github", "prod-writer"]);
        let p = principal(&["github"], false);
        let denied = route(&s, Some(&p), "prod-writer__deploy").unwrap_err();
        let missing = route(&s, Some(&p), "no-such-server__x").unwrap_err();
        assert_eq!(denied.status(), 404, "403 would confirm it exists");
        assert_eq!(denied.status(), missing.status());
        assert_eq!(
            denied.message(),
            missing.message().replace("no-such-server", "prod-writer")
        );
    }

    /// The grant is separate from `model:invoke` on purpose: tools have side
    /// effects and models do not.
    #[test]
    fn invoking_models_does_not_imply_invoking_tool_servers() {
        let s = snap(&["github"]);
        let mut p = principal(&[], false);
        p.allow_all = true; // every model
        assert!(!may_invoke(Some(&p), "github", false));
        assert!(route(&s, Some(&p), "github__search").is_err());
    }

    #[test]
    fn visible_servers_never_carries_the_credential() {
        let s = snap(&["github"]);
        let p = principal(&[], true);
        let v = visible_servers(&s, Some(&p));
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"credential_set\":true"));
        assert!(
            !json.contains("sk-x"),
            "the credential must never be serialised"
        );
    }

    /// The catalogue has to be usable by a model without the caller
    /// translating anything — that is the point of the gateway.
    #[test]
    fn tools_convert_to_the_shape_a_model_is_handed() {
        let tools = tools_from_result(
            "github",
            &serde_json::json!({"result": {"tools": [
                {"name": "search", "description": "d", "inputSchema": {"type": "object"}}]}}),
        );
        let openai = as_openai_tools(&tools);
        assert_eq!(openai[0]["type"], "function");
        assert_eq!(openai[0]["function"]["name"], "github__search");
        assert_eq!(openai[0]["function"]["parameters"]["type"], "object");
    }

    /// One server answering nonsense must not take the others down.
    #[test]
    fn a_server_that_answers_nothing_useful_contributes_nothing() {
        assert!(tools_from_result("x", &serde_json::json!({"error": {"code": -32601}})).is_empty());
        assert!(tools_from_result("x", &serde_json::json!({"result": {}})).is_empty());
    }

    #[test]
    fn a_server_without_a_credential_sends_no_auth_header() {
        let mut def = snap(&["x"]).mcp_servers.remove("x").unwrap();
        def.api_key = None;
        let headers = auth_headers(&def);
        assert!(headers.iter().all(|(k, _)| k != "authorization"));

        def.api_key = Some("tok".into());
        def.auth_scheme = None;
        let raw = auth_headers(&def);
        assert!(raw.contains(&("authorization".to_string(), "tok".to_string())));
    }
}

// ---------------------------------------------------------------- transport

/// Everything the handlers need to reach a server, without `mcp` knowing what
/// an `AppState` is.
pub struct Call<'a> {
    pub server: &'a McpServerDef,
    pub body: serde_json::Value,
}

impl<'a> Call<'a> {
    /// `tools/list` against one server.
    pub fn list(server: &'a McpServerDef) -> Self {
        Self {
            server,
            body: rpc(1, "tools/list", serde_json::json!({})),
        }
    }

    /// `tools/call`, with the namespace already stripped off the tool name —
    /// the server knows its tools by their own names and has never heard of
    /// this gateway's prefix.
    pub fn invoke(server: &'a McpServerDef, tool: &str, arguments: serde_json::Value) -> Self {
        Self {
            server,
            body: rpc(
                2,
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": arguments }),
            ),
        }
    }
}

/// An SSE-framed JSON-RPC reply, reduced to the JSON it carries.
///
/// MCP's streamable-HTTP transport is allowed to answer a plain request with
/// `text/event-stream` holding a single `data:` line, and several published
/// servers do. Handing that to `serde_json` unchanged fails, and the failure
/// looks like the server returned nonsense rather than like a framing
/// difference — so it is unwrapped here, once, where the reason can be
/// written down.
pub fn parse_reply(content_type: Option<&str>, raw: &[u8]) -> Option<serde_json::Value> {
    let text = std::str::from_utf8(raw).ok()?;
    let looks_sse = content_type.is_some_and(|c| c.starts_with("text/event-stream"))
        || text.trim_start().starts_with("event:")
        || text.trim_start().starts_with("data:");
    if !looks_sse {
        return serde_json::from_str(text).ok();
    }
    // The last `data:` line wins: a server may send a comment or an `event:`
    // line first, and only one JSON-RPC reply is ever carried.
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .next_back()
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    /// The tool name that goes upstream is the server's own, never the
    /// gateway's namespaced one.
    #[test]
    fn the_namespace_is_stripped_before_the_call_leaves() {
        let def = McpServerDef {
            name: "github".into(),
            url: "https://x/mcp".into(),
            transport: "http".into(),
            description: String::new(),
            auth_header: "authorization".into(),
            auth_scheme: Some("Bearer".into()),
            api_key: None,
        };
        let call = Call::invoke(&def, "search", serde_json::json!({"q": "x"}));
        assert_eq!(call.body["method"], "tools/call");
        assert_eq!(
            call.body["params"]["name"], "search",
            "the server has never heard of this gateway's prefix"
        );
    }

    /// Several published servers answer a plain POST with a one-line SSE
    /// frame. Failing to parse that reads as "the server returned nonsense".
    #[test]
    fn an_sse_framed_reply_is_unwrapped() {
        let sse =
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let v = parse_reply(Some("text/event-stream"), sse).expect("unwrapped");
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 0);

        // And a plain JSON body still parses as itself.
        let plain = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        assert!(parse_reply(Some("application/json"), plain).is_some());
        // Detected by shape too, for a server that mislabels its content type.
        assert!(parse_reply(None, sse).is_some());
    }

    #[test]
    fn a_reply_that_is_neither_is_none_rather_than_a_panic() {
        assert!(parse_reply(Some("text/html"), b"<html>502</html>").is_none());
        assert!(parse_reply(None, &[0xff, 0xfe]).is_none());
    }
}
