//! `nine_snake::llm` — model gateway and concrete clients.
//!
//! Two responsibilities live here:
//!
//! * [`ollama`] — a thin async HTTP wrapper around the local Ollama
//!   server (`/api/chat`, `/api/generate`, `/api/embeddings`).
//! * [`gateway`] — a higher-level [`LlmGateway`] that handles prompt
//!   caching, request rate limiting, and graceful degradation to a
//!   remote fallback endpoint when the local server is unavailable.
//! * [`anthropic`] — v1.1 P0-1: Anthropic Claude HTTP client.

pub mod gateway;
pub mod ollama;
// v1.1 P0-1: Anthropic Claude provider
pub mod anthropic;

pub use anthropic::{AnthropicClient, Role as AnthropicRole};
pub use gateway::{LlmGateway, StreamToken};
pub use ollama::{ChatMessage, ChatResponse, OllamaClient, Role};
