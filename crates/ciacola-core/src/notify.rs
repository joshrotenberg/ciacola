//! Telling whoever is attached, as `notifications/message`.
//!
//! Same shape stage 10 proved: any MCP client already renders these, so
//! nothing bespoke is needed on the receiving end. When nobody is
//! attached the send drops, which is what ntfy would be for.

use serde_json::json;
use tower_mcp::{LogLevel, LoggingMessageParams, NotificationSender, ServerNotification};

#[derive(Clone)]
pub struct Notifier(pub NotificationSender);

impl Notifier {
    pub fn turn(&self, level: LogLevel, agent_id: &str, seq: i64, state: &str, detail: &str) {
        let _ = self
            .0
            .try_send(ServerNotification::LogMessage(LoggingMessageParams {
                level,
                logger: Some("turns".into()),
                data: json!({
                    "agent_id": agent_id,
                    "seq": seq,
                    "state": state,
                    "detail": detail,
                }),
                meta: None,
            }));
    }
}
