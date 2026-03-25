//! Command router — routes user commands via LLM-based orchestration.
//!
//! All user queries go to the `general` Starflask agent. The agent handles
//! delegation internally via the `delegate` tool in its agentic loop —
//! no parsing or interception needed here.
//!
//! Instead of using the SDK's blocking `query()`, we manage our own polling
//! loop so we can broadcast real-time session progress (iteration logs,
//! tool calls, delegations) via WebSocket.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use starflask::Starflask;
use uuid::Uuid;

use crate::agent_registry::AgentRegistry;
use crate::crypto_executor::{CryptoExecutor, ExecutionResult};
use crate::db::Database;
use crate::gateway::events::EventBroadcaster;
use crate::gateway::protocol::GatewayEvent;
use crate::http::shared_client;
use crate::starflask_bridge;

/// How often to poll the Starflask session (seconds).
const POLL_INTERVAL_SECS: u64 = 3;
/// Maximum time to wait for a session to complete (seconds).
const POLL_TIMEOUT_SECS: u64 = 600;

pub struct CommandRouter {
    registry: Arc<AgentRegistry>,
    #[allow(dead_code)]
    starflask: Arc<Starflask>,
    crypto_executor: Option<Arc<CryptoExecutor>>,
    db: Arc<Database>,
    broadcaster: Arc<EventBroadcaster>,
    /// Starflask API key for raw HTTP calls.
    api_key: String,
    /// Starflask base URL (e.g. "https://starflask.com/api").
    base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub message: String,
    #[serde(default)]
    pub capability: Option<String>,
    #[serde(default)]
    pub hook: Option<String>,
    #[serde(default)]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CommandOutput {
    CryptoExecution { results: Vec<ExecutionResult> },
    MediaGeneration { urls: Vec<String>, media_type: String },
    TextResponse { text: String },
    Raw { data: Value },
}

/// Parsed session result from the Starflask API.
struct SessionResult {
    result: Option<Value>,
    result_summary: Option<String>,
}

impl CommandRouter {
    pub fn new(
        registry: Arc<AgentRegistry>,
        starflask: Arc<Starflask>,
        crypto_executor: Option<Arc<CryptoExecutor>>,
        db: Arc<Database>,
        broadcaster: Arc<EventBroadcaster>,
        api_key: String,
        base_url: String,
    ) -> Self {
        Self { registry, starflask, crypto_executor, db, broadcaster, api_key, base_url }
    }

    // ── Starflask raw HTTP helpers ──────────────────────────────────

    /// POST to Starflask, creating a new session. Returns the raw JSON.
    async fn sf_post(&self, path: &str, body: Value) -> Result<Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = shared_client()
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("Starflask POST {}: {}", path, e))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Starflask POST {} returned {}: {}", path, status, body));
        }

        resp.json::<Value>().await.map_err(|e| format!("Starflask POST {} parse error: {}", path, e))
    }

    /// GET from Starflask. Returns the raw JSON.
    async fn sf_get(&self, path: &str) -> Result<Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = shared_client()
            .get(&url)
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("Starflask GET {}: {}", path, e))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Starflask GET {} returned {}: {}", path, status, body));
        }

        resp.json::<Value>().await.map_err(|e| format!("Starflask GET {} parse error: {}", path, e))
    }

    /// Create a query session on Starflask. Returns session ID.
    async fn create_query_session(&self, agent_id: &Uuid, message: &str) -> Result<Uuid, String> {
        let body = serde_json::json!({ "message": message });
        let resp = self.sf_post(&format!("/agents/{}/query", agent_id), body).await?;
        extract_session_id(&resp)
    }

    /// Fire a hook on Starflask. Returns session ID.
    async fn create_hook_session(&self, agent_id: &Uuid, event: &str, payload: Value) -> Result<Uuid, String> {
        let body = serde_json::json!({ "event": event, "payload": payload });
        let resp = self.sf_post(&format!("/agents/{}/fire_hook", agent_id), body).await?;
        extract_session_id(&resp)
    }

    /// Poll a session until completion, broadcasting iteration log diffs.
    async fn poll_with_progress(
        &self,
        agent_id: &Uuid,
        session_id: &Uuid,
        cmd_id: i64,
    ) -> Result<SessionResult, String> {
        let path = format!("/agents/{}/sessions/{}", agent_id, session_id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(POLL_TIMEOUT_SECS);
        let mut seen_log_count: usize = 0;
        let mut last_status = String::new();

        loop {
            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;

            if tokio::time::Instant::now() > deadline {
                return Err(format!("Session timed out after {}s", POLL_TIMEOUT_SECS));
            }

            let session = match self.sf_get(&path).await {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("[CommandRouter] Poll error (will retry): {}", e);
                    continue;
                }
            };

            let status = session.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");

            // Broadcast status change
            if status != last_status {
                self.broadcaster.broadcast(GatewayEvent::new(
                    "starflask.session_progress",
                    serde_json::json!({
                        "command_id": cmd_id,
                        "session_id": session_id.to_string(),
                        "event": "status_change",
                        "status": status,
                    }),
                ));
                last_status = status.to_string();
            }

            // Broadcast any new log entries
            if let Some(logs) = session.get("logs").and_then(|v| v.as_array()) {
                for entry in logs.iter().skip(seen_log_count) {
                    self.broadcast_log_entry(cmd_id, session_id, entry);
                }
                seen_log_count = logs.len();
            }

            match status {
                "completed" => {
                    return Ok(SessionResult {
                        result: session.get("result").cloned(),
                        result_summary: session.get("result_summary").and_then(|v| v.as_str()).map(String::from),
                    });
                }
                "failed" => {
                    let err = session.get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("Session failed: {}", err));
                }
                _ => continue,
            }
        }
    }

    /// Broadcast a single iteration log entry as a WebSocket event.
    fn broadcast_log_entry(&self, cmd_id: i64, session_id: &Uuid, entry: &Value) {
        let event_type = entry.get("event")
            .or_else(|| entry.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        // Skip noisy heartbeat and delegation_waiting events
        if event_type == "heartbeat" || event_type == "delegation_waiting" {
            return;
        }

        let iteration = entry.get("iteration").and_then(|v| v.as_u64()).unwrap_or(0);

        // Build a human-readable summary of the log entry
        let summary = summarize_log_entry(event_type, entry);

        self.broadcaster.broadcast(GatewayEvent::new(
            "starflask.session_progress",
            serde_json::json!({
                "command_id": cmd_id,
                "session_id": session_id.to_string(),
                "event": event_type,
                "iteration": iteration,
                "summary": summary,
                "raw": entry,
            }),
        ));
    }

    // ── Routing methods ─────────────────────────────────────────────

    /// Route a command through the orchestrator agent.
    pub async fn route(&self, command: Command) -> Result<CommandOutput, String> {
        if let Some(ref cap) = command.capability {
            if !cap.is_empty() {
                return self.route_direct(cap, &command).await;
            }
        }

        let general_id = self.registry.get_agent_id("general")
            .or_else(|| self.registry.get_any_agent_id())
            .ok_or_else(|| "No agents available. Sync your Starflask agents first.".to_string())?;

        let cmd_id = self.db.log_starflask_command("general", None, &command.message).unwrap_or(0);

        self.broadcaster.broadcast(GatewayEvent::new(
            "starflask.command_started",
            serde_json::json!({
                "command_id": cmd_id,
                "capability": "general",
                "message": &command.message,
            }),
        ));

        let session = if let Some(hook) = &command.hook {
            let payload = command.payload.clone().unwrap_or(serde_json::json!({}));
            let session_id = self.create_hook_session(&general_id, hook, payload).await
                .map_err(|e| format!("Hook fire failed: {}", e))?;
            self.poll_with_progress(&general_id, &session_id, cmd_id).await
                .map_err(|e| format!("Hook failed: {}", e))?
        } else {
            let session_id = self.create_query_session(&general_id, &command.message).await
                .map_err(|e| format!("Query failed: {}", e))?;
            self.poll_with_progress(&general_id, &session_id, cmd_id).await
                .map_err(|e| format!("Query failed: {}", e))?
        };

        let output = self.parse_output("general", &session.result, session.result_summary.as_deref()).await;
        self.complete_command(cmd_id, "general", &output, false);
        output
    }

    /// Route directly to a specific agent, bypassing the orchestrator.
    async fn route_direct(&self, capability: &str, command: &Command) -> Result<CommandOutput, String> {
        let agent_id = self.registry.get_agent_id(capability)
            .or_else(|| {
                log::info!("[CommandRouter] No agent for '{}', falling back to any available agent", capability);
                self.registry.get_any_agent_id()
            })
            .ok_or_else(|| "No agents available. Sync your Starflask agents first.".to_string())?;

        let cmd_id = self.db.log_starflask_command(capability, None, &command.message).unwrap_or(0);

        self.broadcaster.broadcast(GatewayEvent::new(
            "starflask.command_started",
            serde_json::json!({
                "command_id": cmd_id,
                "capability": capability,
                "message": &command.message,
            }),
        ));

        let session = if let Some(hook) = &command.hook {
            let payload = command.payload.clone().unwrap_or(serde_json::json!({}));
            let session_id = self.create_hook_session(&agent_id, hook, payload).await
                .map_err(|e| format!("Hook fire failed: {}", e))?;
            self.poll_with_progress(&agent_id, &session_id, cmd_id).await
                .map_err(|e| format!("Hook failed: {}", e))?
        } else {
            let session_id = self.create_query_session(&agent_id, &command.message).await
                .map_err(|e| format!("Query failed: {}", e))?;
            self.poll_with_progress(&agent_id, &session_id, cmd_id).await
                .map_err(|e| format!("Query failed: {}", e))?
        };

        let output = self.parse_output(capability, &session.result, session.result_summary.as_deref()).await;
        self.complete_command(cmd_id, capability, &output, false);
        output
    }

    /// Fire a hook on a capability's agent with delegation support.
    pub async fn fire_hook_with_delegation(
        &self,
        capability: &str,
        hook_event: &str,
        payload: Value,
    ) -> Result<CommandOutput, String> {
        let agent_id = self.registry.get_agent_id(capability)
            .ok_or_else(|| format!("No agent for capability '{}'", capability))?;

        let cmd_id = self.db.log_starflask_command(capability, Some(hook_event), &format!("hook:{}", hook_event)).unwrap_or(0);

        self.broadcaster.broadcast(GatewayEvent::new(
            "starflask.command_started",
            serde_json::json!({
                "command_id": cmd_id,
                "capability": capability,
                "hook_event": hook_event,
            }),
        ));

        let session_id = self.create_hook_session(&agent_id, hook_event, payload).await
            .map_err(|e| format!("Hook fire failed: {}", e))?;
        let session = self.poll_with_progress(&agent_id, &session_id, cmd_id).await
            .map_err(|e| format!("Hook failed: {}", e))?;

        let output = self.parse_output(capability, &session.result, session.result_summary.as_deref()).await;
        self.complete_command(cmd_id, capability, &output, false);
        output
    }

    /// Log completion and broadcast the result event.
    fn complete_command(
        &self,
        cmd_id: i64,
        capability: &str,
        output: &Result<CommandOutput, String>,
        delegated: bool,
    ) {
        let status = if output.is_ok() { "completed" } else { "failed" };
        let result_data = match output {
            Ok(o) => serde_json::to_value(o).unwrap_or(Value::Null),
            Err(e) => serde_json::json!({ "error": e }),
        };
        let _ = self.db.complete_starflask_command(cmd_id, status, &result_data);

        self.broadcaster.broadcast(GatewayEvent::new(
            "starflask.command_completed",
            serde_json::json!({
                "command_id": cmd_id,
                "capability": capability,
                "status": status,
                "delegated": delegated,
                "result": &result_data,
            }),
        ));
    }

    /// Parse session result into typed output.
    async fn parse_output(
        &self,
        capability: &str,
        result: &Option<Value>,
        result_summary: Option<&str>,
    ) -> Result<CommandOutput, String> {
        // 1. Check structured_data first
        if let Some(sd) = starflask_bridge::parse_structured_data(result) {
            match sd.get("type").and_then(|v| v.as_str()) {
                Some("media") => {
                    let urls = sd.get("urls")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                        .unwrap_or_default();
                    if !urls.is_empty() {
                        let media_type = sd.get("media_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("image")
                            .to_string();
                        return Ok(CommandOutput::MediaGeneration { urls, media_type });
                    }
                }
                Some("crypto") => {
                    let instructions = sd.get("instructions")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter()
                            .filter_map(|v| serde_json::from_value(v.clone()).ok())
                            .collect::<Vec<_>>())
                        .unwrap_or_default();
                    if !instructions.is_empty() {
                        let executor = self.crypto_executor.as_ref()
                            .ok_or("Crypto executor not available (no wallet configured)")?;
                        let mut results = Vec::new();
                        for instruction in instructions {
                            match executor.execute(instruction).await {
                                Ok(r) => results.push(r),
                                Err(e) => results.push(ExecutionResult {
                                    success: false,
                                    data: serde_json::json!({ "error": e }),
                                }),
                            }
                        }
                        return Ok(CommandOutput::CryptoExecution { results });
                    }
                }
                _ => {}
            }
        }

        // 2. Use output_type from the pack definition
        let output_type = self.registry.get_output_type(capability);

        match output_type.as_str() {
            t if t.starts_with("media:") => {
                let media_type = t.strip_prefix("media:").unwrap_or("image").to_string();
                let urls = starflask_bridge::parse_media_result(result, result_summary);
                Ok(CommandOutput::MediaGeneration { urls, media_type })
            }

            "crypto" => {
                let instructions = starflask_bridge::parse_session_result(result);
                if instructions.is_empty() {
                    let text = starflask_bridge::parse_text_result(result);
                    return Ok(CommandOutput::TextResponse { text });
                }

                let executor = self.crypto_executor.as_ref()
                    .ok_or("Crypto executor not available (no wallet configured)")?;

                let mut results = Vec::new();
                for instruction in instructions {
                    match executor.execute(instruction).await {
                        Ok(r) => results.push(r),
                        Err(e) => results.push(ExecutionResult {
                            success: false,
                            data: serde_json::json!({ "error": e }),
                        }),
                    }
                }
                Ok(CommandOutput::CryptoExecution { results })
            }

            _ => {
                let media_urls = starflask_bridge::parse_media_result(result, result_summary);
                if !media_urls.is_empty() {
                    return Ok(CommandOutput::MediaGeneration { urls: media_urls, media_type: "image".to_string() });
                }

                let text = starflask_bridge::parse_text_result(result);
                if text.is_empty() {
                    Ok(CommandOutput::Raw { data: result.clone().unwrap_or(Value::Null) })
                } else {
                    let urls = starflask_bridge::extract_urls_from_text(&text);
                    let has_media_url = urls.iter().any(|u| {
                        u.contains(".jpg") || u.contains(".jpeg") || u.contains(".png")
                        || u.contains(".webp") || u.contains(".gif") || u.contains(".mp4")
                        || u.contains("fal.media") || u.contains("replicate.delivery")
                    });
                    if has_media_url {
                        Ok(CommandOutput::MediaGeneration { urls, media_type: "image".to_string() })
                    } else {
                        Ok(CommandOutput::TextResponse { text })
                    }
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Extract session UUID from a Starflask API response.
fn extract_session_id(resp: &Value) -> Result<Uuid, String> {
    let id_str = resp.get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("No session id in response: {}", resp))?;
    Uuid::parse_str(id_str)
        .map_err(|e| format!("Invalid session UUID '{}': {}", id_str, e))
}

/// Build a human-readable summary from a log entry.
fn summarize_log_entry(event_type: &str, entry: &Value) -> String {
    match event_type {
        "assistant_tool_calls" => {
            // Try to extract tool names from the payload
            let tool_names = extract_tool_names(entry);
            if tool_names.is_empty() {
                "Calling tools...".to_string()
            } else if tool_names.iter().any(|n| n == "delegate") {
                // Extract delegation target
                let target = extract_delegation_target(entry);
                format!("Delegating to {}...", target.unwrap_or_else(|| "subagent".to_string()))
            } else {
                format!("Calling {}...", tool_names.join(", "))
            }
        }
        "tool_start" => "Running tool...".to_string(),
        "tool_results" => {
            let tool_names = extract_tool_names(entry);
            if tool_names.is_empty() {
                "Tool result received".to_string()
            } else if tool_names.iter().any(|n| n == "delegate") {
                "Delegation result received".to_string()
            } else {
                format!("{} completed", tool_names.join(", "))
            }
        }
        "assistant_text" => "Thinking...".to_string(),
        "report_result" => {
            let success = entry.get("success")
                .or_else(|| entry.get("payload").and_then(|p| p.get("success")))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if success { "Task completed".to_string() } else { "Task failed".to_string() }
        }
        "llm_error" => "AI error occurred".to_string(),
        _ => format!("{}", event_type),
    }
}

/// Extract tool names from a log entry.
fn extract_tool_names(entry: &Value) -> Vec<String> {
    // Try entry.tool_calls[].name or entry.payload.tool_calls[].name
    let candidates = [
        entry.get("tool_calls"),
        entry.get("payload").and_then(|p| p.get("tool_calls")),
    ];
    for tc in candidates.iter().flatten() {
        if let Some(arr) = tc.as_array() {
            let names: Vec<String> = arr.iter()
                .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }

    // Also try top-level "name" field (for tool_results with single name)
    if let Some(name) = entry.get("name").and_then(|v| v.as_str()) {
        return vec![name.to_string()];
    }
    // Or from the stringified content (e.g., "delegate\n\n{...}")
    if let Some(content) = entry.as_str() {
        let first_line = content.lines().next().unwrap_or("");
        if !first_line.is_empty() && first_line.len() < 50 {
            return vec![first_line.to_string()];
        }
    }
    vec![]
}

/// Extract delegation target agent name from a delegate tool call.
fn extract_delegation_target(entry: &Value) -> Option<String> {
    let candidates = [
        entry.get("tool_calls"),
        entry.get("payload").and_then(|p| p.get("tool_calls")),
    ];
    for tc in candidates.iter().flatten() {
        if let Some(arr) = tc.as_array() {
            for tool in arr {
                if tool.get("name").and_then(|v| v.as_str()) == Some("delegate") {
                    // Arguments might be a JSON string or an object
                    if let Some(args) = tool.get("arguments") {
                        let args_obj = if let Some(s) = args.as_str() {
                            serde_json::from_str::<Value>(s).ok()
                        } else {
                            Some(args.clone())
                        };
                        if let Some(obj) = args_obj {
                            if let Some(name) = obj.get("agent_name").and_then(|v| v.as_str()) {
                                return Some(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}
