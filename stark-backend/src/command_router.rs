//! Command router — routes user commands via LLM-based orchestration.
//!
//! All user queries go to the `general` Starflask agent first. The LLM decides
//! whether to answer directly or return a delegation instruction to a specialist.
//! Hook-driven agents (discord_moderator, telegram_moderator) can also delegate
//! via `fire_hook_with_delegation()`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use starflask::Starflask;
use crate::agent_registry::AgentRegistry;
use crate::crypto_executor::{CryptoExecutor, ExecutionResult};
use crate::db::Database;
use crate::gateway::events::EventBroadcaster;
use crate::gateway::protocol::GatewayEvent;
use crate::starflask_bridge;

/// Capabilities that agents are allowed to delegate to.
const DELEGATABLE_CAPABILITIES: &[&str] = &["crypto", "image_gen", "video_gen"];

/// Maximum delegation depth (no chaining — only 1 level).
const MAX_DELEGATION_DEPTH: u8 = 1;

pub struct CommandRouter {
    registry: Arc<AgentRegistry>,
    starflask: Arc<Starflask>,
    crypto_executor: Option<Arc<CryptoExecutor>>,
    db: Arc<Database>,
    broadcaster: Arc<EventBroadcaster>,
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

impl CommandRouter {
    pub fn new(
        registry: Arc<AgentRegistry>,
        starflask: Arc<Starflask>,
        crypto_executor: Option<Arc<CryptoExecutor>>,
        db: Arc<Database>,
        broadcaster: Arc<EventBroadcaster>,
    ) -> Self {
        Self { registry, starflask, crypto_executor, db, broadcaster }
    }

    /// Route a command through the LLM orchestrator.
    ///
    /// If `command.capability` is explicitly set, bypass the orchestrator and
    /// go directly to that agent (`route_direct`). Otherwise, query the
    /// `general` agent and check whether it delegates to a specialist.
    pub async fn route(&self, command: Command) -> Result<CommandOutput, String> {
        // Manual override — explicit capability set by user
        if let Some(ref cap) = command.capability {
            if !cap.is_empty() {
                return self.route_direct(cap, &command).await;
            }
        }

        // Phase 1: Query the general agent
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
            self.starflask.fire_hook_and_wait(&general_id, hook, payload).await
                .map_err(|e| format!("Hook fire failed: {}", e))?
        } else {
            self.starflask.query(&general_id, &command.message).await
                .map_err(|e| format!("Query failed: {}", e))?
        };

        // Phase 2: Check for delegation
        if let Some(output) = self.try_delegate("general", &session, 0).await {
            let result = output;
            self.complete_command(cmd_id, "general", &result, true);
            return result;
        }

        // Phase 3: No delegation — return general agent's response as text
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
            self.starflask.fire_hook_and_wait(&agent_id, hook, payload).await
                .map_err(|e| format!("Hook fire failed: {}", e))?
        } else {
            self.starflask.query(&agent_id, &command.message).await
                .map_err(|e| format!("Query failed: {}", e))?
        };

        // Check for delegation (allows any agent to delegate, not just general)
        if let Some(output) = self.try_delegate(capability, &session, 0).await {
            let result = output;
            self.complete_command(cmd_id, capability, &result, true);
            return result;
        }

        let output = self.parse_output(capability, &session.result, session.result_summary.as_deref()).await;
        self.complete_command(cmd_id, capability, &output, false);
        output
    }

    /// Try to follow a delegation instruction from a session result.
    ///
    /// Returns `Some(result)` if delegation was found and executed, `None` otherwise.
    async fn try_delegate(
        &self,
        from_capability: &str,
        session: &starflask::Session,
        depth: u8,
    ) -> Option<Result<CommandOutput, String>> {
        if depth >= MAX_DELEGATION_DEPTH {
            log::warn!("[CommandRouter] Max delegation depth reached, stopping chain");
            return None;
        }

        let delegation = starflask_bridge::parse_delegation_result(&session.result)?;

        if !DELEGATABLE_CAPABILITIES.contains(&delegation.delegate.as_str()) {
            log::warn!(
                "[CommandRouter] '{}' tried to delegate to invalid target '{}', treating as text",
                from_capability, delegation.delegate
            );
            return None;
        }

        log::info!(
            "[CommandRouter] Delegation to '{}' from '{}': {}",
            delegation.delegate, from_capability, delegation.message
        );

        self.broadcaster.broadcast(GatewayEvent::new(
            "starflask.delegation",
            serde_json::json!({
                "from": from_capability,
                "to": &delegation.delegate,
                "message": &delegation.message,
            }),
        ));

        let delegate_id = match self.registry.get_agent_id(&delegation.delegate) {
            Some(id) => id,
            None => return Some(Err(format!(
                "Delegation target '{}' has no registered agent. Sync your agents.",
                delegation.delegate
            ))),
        };

        let delegate_session = match self.starflask.query(&delegate_id, &delegation.message).await {
            Ok(s) => s,
            Err(e) => return Some(Err(format!(
                "Delegated query to '{}' failed: {}", delegation.delegate, e
            ))),
        };

        let output = self.parse_output(
            &delegation.delegate,
            &delegate_session.result,
            delegate_session.result_summary.as_deref(),
        ).await;

        Some(output)
    }

    /// Fire a hook on a capability's agent, then check for delegation.
    ///
    /// Used by the hook endpoint so that hook-driven agents (discord_moderator, etc.)
    /// can delegate to specialists (image_gen, crypto, etc.) and deliver the result
    /// back to the source agent for posting.
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

        let session = self.starflask.fire_hook_and_wait(&agent_id, hook_event, payload.clone()).await
            .map_err(|e| format!("Hook fire failed: {}", e))?;

        // Check if the hook agent wants to delegate
        if let Some(delegation) = starflask_bridge::parse_delegation_result(&session.result) {
            if DELEGATABLE_CAPABILITIES.contains(&delegation.delegate.as_str()) {
                log::info!(
                    "[CommandRouter] Hook agent '{}' delegated to '{}': {}",
                    capability, delegation.delegate, delegation.message
                );

                self.broadcaster.broadcast(GatewayEvent::new(
                    "starflask.delegation",
                    serde_json::json!({
                        "from": capability,
                        "to": &delegation.delegate,
                        "message": &delegation.message,
                    }),
                ));

                let delegate_id = self.registry.get_agent_id(&delegation.delegate)
                    .ok_or_else(|| format!(
                        "Delegation target '{}' has no registered agent. Sync your agents.",
                        delegation.delegate
                    ))?;

                let delegate_session = self.starflask.query(&delegate_id, &delegation.message).await
                    .map_err(|e| format!("Delegated query to '{}' failed: {}", delegation.delegate, e))?;

                let delegate_output = self.parse_output(
                    &delegation.delegate,
                    &delegate_session.result,
                    delegate_session.result_summary.as_deref(),
                ).await;

                // Build a delivery message and send it back to the source agent
                // so it can post the result (e.g. via discord_send_message)
                let delivery_msg = Self::build_delivery_message(
                    &delegation.delegate, &delegate_output, &payload,
                );

                log::info!(
                    "[CommandRouter] Delivering delegation result back to '{}' agent",
                    capability
                );

                // Query the source agent with the delivery message
                let _delivery_session = self.starflask.query(&agent_id, &delivery_msg).await
                    .map_err(|e| format!("Delivery query to '{}' failed: {}", capability, e))?;

                self.complete_command(cmd_id, &delegation.delegate, &delegate_output, true);
                return delegate_output;
            } else {
                log::warn!(
                    "[CommandRouter] Hook agent '{}' tried to delegate to invalid target '{}'",
                    capability, delegation.delegate
                );
            }
        }

        // No delegation — return the hook session result directly
        let output = self.parse_output(capability, &session.result, session.result_summary.as_deref()).await;
        self.complete_command(cmd_id, capability, &output, false);
        output
    }

    /// Build a delivery message instructing the source agent to post the sub-agent's result.
    fn build_delivery_message(
        delegate_capability: &str,
        delegate_output: &Result<CommandOutput, String>,
        original_payload: &Value,
    ) -> String {
        let result_text = match delegate_output {
            Ok(CommandOutput::MediaGeneration { urls, media_type }) => {
                let url_list = urls.join("\n");
                format!("The {} agent generated the following {}(s):\n{}", delegate_capability, media_type, url_list)
            }
            Ok(CommandOutput::TextResponse { text }) => {
                format!("The {} agent responded: {}", delegate_capability, text)
            }
            Ok(CommandOutput::CryptoExecution { results }) => {
                format!("The {} agent executed {} transaction(s). Results: {:?}", delegate_capability, results.len(), results)
            }
            Ok(CommandOutput::Raw { data }) => {
                format!("The {} agent returned: {}", delegate_capability, data)
            }
            Err(e) => {
                format!("The {} agent encountered an error: {}", delegate_capability, e)
            }
        };

        // Extract Discord/Telegram context from the original hook payload
        let channel_id = original_payload.get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let message_id = original_payload.get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let mut msg = format!(
            "DELEGATION RESULT — You previously delegated a request to the {} agent. \
             Please deliver this result to the user by replying in the conversation.\n\n{}\n",
            delegate_capability, result_text
        );

        if !channel_id.is_empty() {
            msg.push_str(&format!(
                "\nContext: channel_id={}, message_id={}. \
                 Use `discord_send_message` or `telegram_send_message` to reply with the result, \
                 using reply_to/reply_to_message_id to thread it to the original message.",
                channel_id, message_id
            ));
        }

        msg
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

    /// Parse session result into typed output based on capability.
    async fn parse_output(
        &self,
        capability: &str,
        result: &Option<Value>,
        result_summary: Option<&str>,
    ) -> Result<CommandOutput, String> {
        match capability {
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

            "image_gen" => {
                let urls = starflask_bridge::parse_media_result(result, result_summary);
                Ok(CommandOutput::MediaGeneration { urls, media_type: "image".to_string() })
            }

            "video_gen" => {
                let urls = starflask_bridge::parse_media_result(result, result_summary);
                Ok(CommandOutput::MediaGeneration { urls, media_type: "video".to_string() })
            }

            _ => {
                let text = starflask_bridge::parse_text_result(result);
                if text.is_empty() {
                    Ok(CommandOutput::Raw { data: result.clone().unwrap_or(Value::Null) })
                } else {
                    Ok(CommandOutput::TextResponse { text })
                }
            }
        }
    }
}
