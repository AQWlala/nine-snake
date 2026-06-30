use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const DEFAULT_SESSION_TTL_SECS: u64 = 86400;
const RATE_LIMIT_INTERVAL_SECS: u64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebChatSession {
    pub token: String,
    pub created_at: i64,
    pub last_message_at: Option<i64>,
    pub message_count: u64,
}

impl WebChatSession {
    pub fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        now - self.created_at > DEFAULT_SESSION_TTL_SECS as i64
    }

    pub fn can_send_message(&self) -> bool {
        match self.last_message_at {
            Some(t) => {
                let now = chrono::Utc::now().timestamp();
                now - t >= RATE_LIMIT_INTERVAL_SECS as i64
            }
            None => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebChatService {
    sessions: Arc<DashMap<String, WebChatSession>>,
}

impl WebChatService {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }

    pub fn create_session(&self) -> String {
        let token = Uuid::new_v4().to_string();
        let session = WebChatSession {
            token: token.clone(),
            created_at: chrono::Utc::now().timestamp(),
            last_message_at: None,
            message_count: 0,
        };
        self.sessions.insert(token.clone(), session);
        token
    }

    pub fn validate_session(&self, token: &str) -> bool {
        self.sessions
            .get(token)
            .map(|s| !s.is_expired())
            .unwrap_or(false)
    }

    pub fn record_message(&self, token: &str) -> Result<(), String> {
        let mut session = self
            .sessions
            .get_mut(token)
            .ok_or_else(|| "session not found".to_string())?;

        if session.is_expired() {
            return Err("session expired".to_string());
        }

        if !session.can_send_message() {
            return Err("rate limited: wait 10s between messages".to_string());
        }

        session.last_message_at = Some(chrono::Utc::now().timestamp());
        session.message_count += 1;
        Ok(())
    }

    pub fn cleanup_expired(&self) -> usize {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|entry| entry.value().is_expired())
            .map(|entry| entry.key().clone())
            .collect();
        let count = expired.len();
        for token in expired {
            self.sessions.remove(&token);
        }
        count
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions.len()
    }
}

impl Default for WebChatService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_validate_session() {
        let svc = WebChatService::new();
        let token = svc.create_session();
        assert!(svc.validate_session(&token));
        assert!(!svc.validate_session("invalid"));
    }

    #[test]
    fn record_message_increments_count() {
        let svc = WebChatService::new();
        let token = svc.create_session();
        svc.record_message(&token).unwrap();
        let session = svc.sessions.get(&token).unwrap();
        assert_eq!(session.message_count, 1);
    }
}
