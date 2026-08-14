//! The A2A gateway: one address in front of every agent.
//!
//! # What this is for
//!
//! The same argument as [`crate::mcp`], one step further out. An agent is a
//! thing that *acts* — it runs, it calls tools, it costs money — so "which of
//! our keys may set which agent running" is a question somebody eventually has
//! to answer. Here it is `agent:invoke` on `agent/<name>`, checked against the
//! same snapshot as everything else.
//!
//! # Versions are pinned, never inferred
//!
//! A2A 0.3 discriminates objects by `kind`; 1.0 uses protobuf JSON envelopes
//! with PascalCase method names. A gateway can guess which one a client wants
//! from the method name it used, and LiteLLM does — but a guess means the
//! agent card can say one thing while a response is the other, and a client
//! that has already branched on the card is then wrong in a way that looks
//! like the agent misbehaving.
//!
//! So `protocol_version` is a column, and this gateway forwards. It does not
//! translate between the two: see `docs/agents.md` for why that is stated
//! rather than quietly not done.

use serde::Serialize;

use crate::snapshot::{A2aAgentDef, Principal, Snapshot};

/// What a caller may reach.
#[derive(Debug, Clone, Serialize)]
pub struct AgentView {
    pub name: String,
    pub description: String,
    pub protocol_version: String,
    /// Never the credential, only whether one is set.
    pub credential_set: bool,
}

/// One set lookup, like every other authorisation decision on this path.
pub fn may_invoke(principal: Option<&Principal>, agent: &str, open: bool) -> bool {
    match principal {
        Some(p) => p.allow_all_agents || p.allowed_agents.contains(agent),
        None => open,
    }
}

/// The agents this principal may invoke.
pub fn visible_agents(snap: &Snapshot, principal: Option<&Principal>) -> Vec<AgentView> {
    let mut out: Vec<AgentView> = snap
        .a2a_agents
        .values()
        .filter(|a| may_invoke(principal, &a.name, snap.open))
        .map(|a| AgentView {
            name: a.name.clone(),
            description: a.description.clone(),
            protocol_version: a.protocol_version.clone(),
            credential_set: a.api_key.is_some(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Resolve a name to the agent that should receive the call.
///
/// `None` covers "no such agent" and "not yours" alike, and answers 404 for
/// both — an unauthorised caller must not be able to enumerate what this
/// deployment runs, which is the same rule the MCP gateway follows.
pub fn route<'a>(
    snap: &'a Snapshot,
    principal: Option<&Principal>,
    name: &str,
) -> Option<&'a A2aAgentDef> {
    let agent = snap.a2a_agents.get(name)?;
    may_invoke(principal, name, snap.open).then_some(agent)
}

/// Headers to present upstream, built exactly as an MCP server's are.
pub fn auth_headers(agent: &A2aAgentDef) -> Vec<(String, String)> {
    let mut headers = vec![(
        "accept".to_string(),
        "application/json, text/event-stream".to_string(),
    )];
    if let Some(key) = &agent.api_key {
        let value = match agent.auth_scheme.as_deref() {
            Some(scheme) if !scheme.is_empty() => format!("{scheme} {key}"),
            _ => key.clone(),
        };
        headers.push((agent.auth_header.clone(), value));
    }
    headers
}

/// The upstream URL for an agent's card.
///
/// Two spellings exist in the wild and neither is universal, so the caller
/// tries them in order rather than picking one and being wrong for half the
/// ecosystem.
pub fn card_urls(agent: &A2aAgentDef) -> [String; 2] {
    let base = agent.url.trim_end_matches('/');
    [
        format!("{base}/.well-known/agent-card.json"),
        format!("{base}/.well-known/agent.json"),
    ]
}

/// Rewrite the `url` in an agent card to point at this gateway.
///
/// The point of the whole exercise: a client that fetches a card and then
/// talks to whatever `url` it names would go straight to the agent, past the
/// key check and the spend attribution. A card served through here has to
/// describe *here*.
pub fn rewrite_card(card: &mut serde_json::Value, agent: &A2aAgentDef, gateway_url: &str) {
    if let Some(obj) = card.as_object_mut() {
        obj.insert(
            "url".to_string(),
            serde_json::Value::String(format!(
                "{}/v1/agents/{}",
                gateway_url.trim_end_matches('/'),
                agent.name
            )),
        );
        // Pinned, so a card and the responses that follow it agree.
        obj.insert(
            "protocolVersion".to_string(),
            serde_json::Value::String(agent.protocol_version.clone()),
        );
    }
}

/// Methods this gateway forwards, and the ones it refuses.
///
/// A closed list, for the reason `GRANTABLE_VERBS` is one: an unknown method
/// forwarded blind is a request whose effects nobody here can describe, on a
/// credential the caller never sees. Adding one is a line, once somebody can
/// say what it does.
pub const FORWARDED_METHODS: &[&str] = &[
    // 0.3
    "message/send",
    "message/stream",
    "tasks/get",
    "tasks/list",
    "tasks/cancel",
    "tasks/resubscribe",
    "agent/getAuthenticatedExtendedCard",
    // 1.0
    "SendMessage",
    "SendStreamingMessage",
    "GetTask",
    "ListTasks",
    "CancelTask",
    "TaskSubscription",
    "GetAgentCard",
];

/// Whether a JSON-RPC body names a method this gateway will forward.
pub fn method_is_forwarded(body: &serde_json::Value) -> Option<&str> {
    let method = body.get("method")?.as_str()?;
    FORWARDED_METHODS
        .iter()
        .find(|m| **m == method)
        .map(|m| &**m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn agent(name: &str) -> A2aAgentDef {
        A2aAgentDef {
            name: name.to_string(),
            url: "https://agent.example/a2a/".to_string(),
            description: String::new(),
            protocol_version: "0.3".to_string(),
            auth_header: "authorization".to_string(),
            auth_scheme: Some("Bearer".to_string()),
            api_key: Some("sk-secret".to_string()),
        }
    }

    fn snap(names: &[&str]) -> Snapshot {
        Snapshot {
            a2a_agents: names
                .iter()
                .map(|n| (n.to_string(), agent(n)))
                .collect::<HashMap<_, _>>(),
            ..Snapshot::default()
        }
    }

    fn principal(agents: &[&str], all: bool) -> Principal {
        Principal {
            allowed_agents: agents.iter().map(|s| s.to_string()).collect(),
            allow_all_agents: all,
            ..Principal::default()
        }
    }

    /// The whole reason to put a gateway in front: a card that still points at
    /// the agent sends the next request past the key check.
    #[test]
    fn a_served_card_points_at_this_gateway_not_the_agent() {
        let a = agent("planner");
        let mut card = serde_json::json!({
            "name": "Planner",
            "url": "https://agent.example/a2a/",
            "protocolVersion": "1.0",
            "skills": []
        });
        rewrite_card(&mut card, &a, "https://gateway.example:4000/");
        assert_eq!(
            card["url"],
            "https://gateway.example:4000/v1/agents/planner"
        );
        // And the card agrees with what the agent is pinned to, rather than
        // repeating whatever the upstream claimed.
        assert_eq!(card["protocolVersion"], "0.3");
        assert_eq!(card["name"], "Planner", "the rest of the card is untouched");
    }

    #[test]
    fn an_agent_the_caller_cannot_reach_is_indistinguishable_from_one_that_does_not_exist() {
        let s = snap(&["planner", "deployer"]);
        let p = principal(&["planner"], false);
        assert!(route(&s, Some(&p), "planner").is_some());
        assert!(route(&s, Some(&p), "deployer").is_none());
        assert!(route(&s, Some(&p), "no-such-agent").is_none());
    }

    /// An agent acts. Being allowed to invoke every model says nothing about
    /// that.
    #[test]
    fn invoking_models_does_not_imply_invoking_agents() {
        let s = snap(&["deployer"]);
        let mut p = principal(&[], false);
        p.allow_all = true;
        assert!(route(&s, Some(&p), "deployer").is_none());
    }

    #[test]
    fn a_view_never_carries_the_credential() {
        let s = snap(&["planner"]);
        let json = serde_json::to_string(&visible_agents(&s, Some(&principal(&[], true)))).unwrap();
        assert!(json.contains("\"credential_set\":true"));
        assert!(!json.contains("sk-secret"));
    }

    /// Both card spellings are tried, because neither is universal.
    #[test]
    fn both_well_known_card_paths_are_offered_and_the_base_slash_does_not_double() {
        let urls = card_urls(&agent("planner"));
        assert_eq!(
            urls[0],
            "https://agent.example/a2a/.well-known/agent-card.json"
        );
        assert_eq!(urls[1], "https://agent.example/a2a/.well-known/agent.json");
    }

    /// An unknown method is refused rather than forwarded blind: it would be a
    /// request whose effects nobody here can describe, made with a credential
    /// the caller never sees.
    #[test]
    fn only_known_methods_are_forwarded() {
        for m in ["message/send", "SendMessage", "tasks/cancel"] {
            let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": m});
            assert_eq!(method_is_forwarded(&body), Some(m));
        }
        for m in ["admin/deleteEverything", "", "message/sendx"] {
            let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": m});
            assert_eq!(
                method_is_forwarded(&body),
                None,
                "{m} must not be forwarded"
            );
        }
        assert_eq!(method_is_forwarded(&serde_json::json!({"id": 1})), None);
    }
}
