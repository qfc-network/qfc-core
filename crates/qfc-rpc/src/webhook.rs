//! Miner webhook notification system
//!
//! Delivers event notifications to registered webhook URLs when miners
//! receive proofs, rewards, or other events. Supports Discord, Telegram,
//! and custom HTTPS endpoints.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// A registered webhook endpoint
#[derive(Clone, Debug)]
pub struct Webhook {
    /// Unique ID (hex hash of miner_address + url)
    pub id: String,
    /// Full webhook URL
    pub url: String,
    /// Event types to receive
    pub events: Vec<String>,
    /// Registration timestamp (seconds)
    pub created_at: u64,
    /// Whether currently active
    pub active: bool,
}

/// Webhook event payload sent to endpoints
#[derive(Clone, Debug, serde::Serialize)]
pub struct WebhookPayload {
    pub event_type: String,
    pub miner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flops: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward_wei: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot_checked: Option<bool>,
    pub timestamp: u64,
    pub message: String,
}

/// Thread-safe webhook store: miner_address -> Vec<Webhook>
pub type WebhookStore = Arc<RwLock<HashMap<qfc_types::Address, Vec<Webhook>>>>;

/// Create a new empty webhook store
pub fn new_store() -> WebhookStore {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Mask a URL for display (show host, hide path/token)
pub fn mask_url(url: &str) -> String {
    if let Some(idx) = url.find("://") {
        let after_scheme = &url[idx + 3..];
        if let Some(slash) = after_scheme.find('/') {
            let host = &after_scheme[..slash];
            format!("{}://{}/*****", &url[..idx], host)
        } else {
            url.to_string()
        }
    } else {
        "***".to_string()
    }
}

/// Deliver a webhook payload to all registered endpoints for a miner.
/// Runs asynchronously via curl subprocess (fire-and-forget).
pub fn deliver(store: &WebhookStore, miner: &qfc_types::Address, payload: WebhookPayload) {
    let hooks = {
        let map = store.read();
        match map.get(miner) {
            Some(hooks) => hooks
                .iter()
                .filter(|h| {
                    h.active
                        && (h.events.contains(&"all".to_string())
                            || h.events.contains(&payload.event_type))
                })
                .cloned()
                .collect::<Vec<_>>(),
            None => return,
        }
    };

    if hooks.is_empty() {
        return;
    }

    let json = match serde_json::to_string(&payload) {
        Ok(j) => j,
        Err(e) => {
            warn!("Failed to serialize webhook payload: {}", e);
            return;
        }
    };

    for hook in hooks {
        let json = json.clone();
        let url = hook.url.clone();
        let hook_id = hook.id.clone();

        // Fire-and-forget delivery via curl
        tokio::spawn(async move {
            deliver_single(&url, &json, &hook_id).await;
        });
    }
}

async fn deliver_single(url: &str, json: &str, hook_id: &str) {
    // Detect Discord/Telegram and format payload accordingly
    let (final_url, final_body) = if url.contains("discord.com/api/webhooks") {
        // Discord: wrap in embed
        let discord_body = format!(
            r#"{{"embeds":[{{"title":"QFC Miner Event","description":{},"color":5763719}}]}}"#,
            serde_json::to_string(json).unwrap_or_else(|_| format!("\"{}\"", json))
        );
        (url.to_string(), discord_body)
    } else if url.contains("api.telegram.org/bot") {
        // Telegram: extract chat_id from URL query or use as-is
        (url.to_string(), json.to_string())
    } else {
        (url.to_string(), json.to_string())
    };

    let result = tokio::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &final_body,
            "--max-time",
            "10",
            &final_url,
        ])
        .output()
        .await;

    match result {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout);
            if status.starts_with('2') {
                debug!("Webhook {} delivered successfully ({})", hook_id, status);
            } else {
                warn!(
                    "Webhook {} delivery failed: HTTP {}",
                    hook_id,
                    status.trim()
                );
            }
        }
        Err(e) => {
            warn!("Webhook {} delivery error: {}", hook_id, e);
        }
    }
}
