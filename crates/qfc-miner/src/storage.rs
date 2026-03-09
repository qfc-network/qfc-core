//! Persistent earnings storage (JSON file)

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EarningsStore {
    /// Lifetime stats across all sessions
    pub lifetime_tasks_completed: u64,
    pub lifetime_tasks_failed: u64,
    pub lifetime_earnings_wei: u128,
    pub lifetime_flops: u64,
    /// Per-session history (last 100 sessions)
    pub sessions: Vec<SessionRecord>,
    /// Per-task earnings (last 1000 records, newest first)
    pub task_records: Vec<TaskRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRecord {
    pub started_at: u64,
    pub ended_at: u64,
    pub tasks_completed: u64,
    pub tasks_failed: u64,
    pub earnings_wei: u128,
    pub flops: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRecord {
    pub timestamp: u64,
    pub task_id: String,
    pub task_type: String,
    pub model: String,
    pub duration_ms: u64,
    pub reward_wei: u128,
    pub accepted: bool,
    pub epoch: u64,
}

impl EarningsStore {
    /// Load from disk, or create empty if not found
    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save to disk (atomic write via temp file)
    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize earnings: {}", e))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &data).map_err(|e| format!("Failed to write earnings file: {}", e))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("Failed to rename earnings file: {}", e))?;
        Ok(())
    }

    /// Record a completed task
    pub fn record_task(&mut self, record: TaskRecord) {
        if record.accepted {
            self.lifetime_tasks_completed += 1;
            self.lifetime_earnings_wei += record.reward_wei;
        } else {
            self.lifetime_tasks_failed += 1;
        }
        self.lifetime_flops += record.duration_ms; // approximate
        self.task_records.insert(0, record);
        // Keep last 1000 records
        self.task_records.truncate(1000);
    }

    /// Start a new session
    pub fn start_session(&mut self, now: u64) {
        self.sessions.push(SessionRecord {
            started_at: now,
            ended_at: 0,
            tasks_completed: 0,
            tasks_failed: 0,
            earnings_wei: 0,
            flops: 0,
        });
        // Keep last 100 sessions
        self.sessions.truncate(100);
    }

    /// Update current session stats
    pub fn update_session(&mut self, tasks_ok: u64, tasks_fail: u64, earnings: u128, flops: u64) {
        if let Some(session) = self.sessions.last_mut() {
            session.tasks_completed = tasks_ok;
            session.tasks_failed = tasks_fail;
            session.earnings_wei = earnings;
            session.flops = flops;
        }
    }

    /// End current session
    pub fn end_session(&mut self, now: u64) {
        if let Some(session) = self.sessions.last_mut() {
            session.ended_at = now;
        }
    }

    /// Get earnings for last N hours
    pub fn earnings_last_hours(&self, hours: u64) -> u128 {
        let cutoff = now_secs().saturating_sub(hours * 3600);
        self.task_records
            .iter()
            .filter(|r| r.timestamp >= cutoff && r.accepted)
            .map(|r| r.reward_wei)
            .sum()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
