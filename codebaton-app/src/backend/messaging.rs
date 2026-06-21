use std::path::Path;

use codebaton_core::Result;
use codebaton_transport::TextMessagePayload;

use super::Backend;
use super::{
    app_log, append_json_line, control_connection_for_peer, epoch_millis_now_u64,
    normalize_epoch_millis, read_jsonl, send_text_message,
};

impl Backend {
    pub fn send_text_message(&self, peer_name: &str, content: String) -> Result<()> {
        let (endpoint, tls, message) = {
            let g = self.inner.lock().unwrap();
            let (endpoint, tls) = control_connection_for_peer(&g, peer_name)?;
            let message = TextMessagePayload {
                sender_name: g.config.device.name.clone(),
                content,
                timestamp: epoch_millis_now_u64(),
            };
            (endpoint, tls, message)
        };
        send_text_message(endpoint, tls, message.clone())?;
        let config_path = self.config_path();
        record_text_message_history(&config_path, Some(peer_name), &message, true);
        Ok(())
    }

    pub fn take_pending_text_message(&self) -> Option<TextMessagePayload> {
        self.pending_text_messages.lock().unwrap().pop_front()
    }

    pub fn text_messages(&self, peer_name: Option<&str>) -> Vec<serde_json::Value> {
        let path = self.config_path().with_file_name("chat_history.jsonl");
        read_jsonl(&path)
            .into_iter()
            .filter(|row| {
                peer_name
                    .map(|peer| row.get("peerName").and_then(|v| v.as_str()) == Some(peer))
                    .unwrap_or(true)
            })
            .collect()
    }
}

pub(crate) fn record_text_message_history(
    config_path: &Path,
    peer_name: Option<&str>,
    message: &TextMessagePayload,
    mine: bool,
) {
    let path = config_path.with_file_name("chat_history.jsonl");
    let peer_name = peer_name.unwrap_or(&message.sender_name);
    let entry = serde_json::json!({
        "timestamp": normalize_epoch_millis(message.timestamp),
        "peerName": peer_name,
        "senderName": message.sender_name,
        "content": message.content,
        "mine": mine,
    });
    match append_json_line(&path, &entry) {
        Ok(()) => app_log(
            "chat_store_appended",
            &[
                ("peer", peer_name.to_string()),
                ("sender", message.sender_name.clone()),
                ("bytes", message.content.len().to_string()),
            ],
        ),
        Err(error) => app_log(
            "chat_store_append_failed",
            &[
                ("peer", peer_name.to_string()),
                ("sender", message.sender_name.clone()),
                ("error", error.to_string()),
            ],
        ),
    }
}
