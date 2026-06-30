//! v1.0: user-friendly error rendering.
//!
//! Each Tauri command already returns a structured
//! [`crate::commands::CommandError`] envelope.  This module maps
//! that envelope to a small set of user-facing "cards" the
//! front-end can display in the toast / error boundary:
//!
//! * **Network** — Ollama offline, gRPC unreachable.
//! * **Storage** — disk full, sqlite locked, lance corruption.
//! * **Validation** — bad input, missing field, too long.
//! * **Permission** — path traversal, command not in whitelist.
//! * **Internal** — last-resort; we show a generic message and
//!   log the full chain to `tracing`.
//!
//! The front-end does *not* parse these strings for branching;
//! it just shows them.  The structured `code` is the stable
//! contract.

use serde::{Deserialize, Serialize};

use crate::commands::{CommandError, ErrorCode};

/// Categorical error kind — the "card" the front-end renders.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    Network,
    Storage,
    Validation,
    Permission,
    NotFound,
    Internal,
    Unknown,
}

impl ErrorKind {
    pub fn from_code(code: ErrorCode) -> Self {
        match code {
            ErrorCode::Db | ErrorCode::Lance | ErrorCode::Memory => ErrorKind::Storage,
            ErrorCode::Llm => ErrorKind::Network,
            ErrorCode::Swarm => ErrorKind::Network,
            ErrorCode::Validation => ErrorKind::Validation,
            ErrorCode::Permission => ErrorKind::Permission,
            ErrorCode::NotFound => ErrorKind::NotFound,
            ErrorCode::Internal | ErrorCode::Unavailable => ErrorKind::Internal,
        }
    }
}

/// A flat, presentational error card.  The front-end renders
/// `title` in a header and `body` below it; the optional
/// `hint` shows suggested next steps (e.g. "Start Ollama and
/// retry").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorCard {
    pub kind: ErrorKind,
    pub title: String,
    pub body: String,
    pub hint: Option<String>,
    /// Original machine-readable code; kept around for telemetry.
    pub code: String,
}

impl ErrorCard {
    /// Build an error card from a Tauri [`CommandError`].
    pub fn from_command_error(err: &CommandError) -> Self {
        let kind = ErrorKind::from_code(err.code);
        let (title, hint) = match kind {
            ErrorKind::Network => (
                "We can't reach the model".to_string(),
                Some("Is Ollama running on the configured URL?".to_string()),
            ),
            ErrorKind::Storage => (
                "Local storage error".to_string(),
                Some("Check disk space and the data directory permissions.".to_string()),
            ),
            ErrorKind::Validation => ("Invalid input".to_string(), None),
            ErrorKind::Permission => (
                "Action not allowed".to_string(),
                Some("Review the workspace root and shell whitelist.".to_string()),
            ),
            ErrorKind::NotFound => ("Not found".to_string(), None),
            ErrorKind::Internal => (
                "Something went wrong".to_string(),
                Some("The full error has been written to the log.".to_string()),
            ),
            ErrorKind::Unknown => ("Unexpected error".to_string(), None),
        };
        Self {
            kind,
            title,
            body: err.message.clone(),
            hint,
            code: format!("{:?}", err.code).to_ascii_lowercase(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_code_maps_to_network() {
        assert_eq!(ErrorKind::from_code(ErrorCode::Llm), ErrorKind::Network);
    }

    #[test]
    fn permission_code_maps_to_permission() {
        assert_eq!(
            ErrorKind::from_code(ErrorCode::Permission),
            ErrorKind::Permission
        );
    }

    #[test]
    fn db_code_maps_to_storage() {
        assert_eq!(ErrorKind::from_code(ErrorCode::Db), ErrorKind::Storage);
    }

    #[test]
    fn card_includes_title_and_body() {
        let e = CommandError::validation("title is required");
        let c = ErrorCard::from_command_error(&e);
        assert_eq!(c.kind, ErrorKind::Validation);
        assert!(!c.body.is_empty());
    }
}
