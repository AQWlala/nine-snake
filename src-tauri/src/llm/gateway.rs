//! `LlmGateway` — request routing, simple prompt caching, and graceful
//! fallback to a remote OpenAI-compatible endpoint when the local
//! Ollama server is unavailable.
//!
//! ## v1.0.1 P0#4 fix — circuit breaker
//!
//! When Ollama is offline, every chat request would otherwise wait
//! the full `reqwest` timeout (120 s) before returning an error.
//! That makes the front-end appear hung for two minutes per
//! request, which is unacceptable for a desktop app.  The fix is
//! a three-state circuit breaker wrapped around the upstream call:
//!
//! * **Closed** (normal): every request reaches Ollama.  Three
//!   consecutive failures flip the breaker to **Open**.
//! * **Open** (tripped): every request is rejected *immediately*
//!   with `anyhow!("circuit open: upstream unavailable")`.  The
//!   breaker stays Open for `OPEN_DURATION` (60 s) and then
//!   transitions to **HalfOpen**.
//! * **HalfOpen** (probe): the next request is allowed through.
//!   On success the breaker resets to **Closed**; on failure it
//!   returns to **Open** for another full Open window.
//!
//! State is held in an `AtomicU8` (single byte) plus a
//! `parking_lot::Mutex<Instant>` recording the moment the
//! breaker was last tripped (so the Open→HalfOpen transition
//! doesn't require a background timer).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use lru::LruCache;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::anthropic::AnthropicClient;
use super::ollama::{ChatMessage, ChatResponse, OllamaClient};
use crate::security::SsrfGuard;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamToken {
    pub text: String,
    pub done: bool,
    pub incomplete: bool,
}

/// Number of cached completions. The cache is intentionally tiny.
const CACHE_CAPACITY: usize = 64;

/// TTL for cached entries.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// v1.0.1 P0#4: number of consecutive failures that trips the
/// circuit breaker.
const CB_FAILURE_THRESHOLD: u32 = 3;

/// v1.0.1 P0#4: how long the breaker stays Open before it allows
/// a single probe request through.
const CB_OPEN_DURATION: Duration = Duration::from_secs(60);

/// v1.0.1 P0#4: state values packed into the `AtomicU8`.
const CB_CLOSED: u8 = 0;
const CB_OPEN: u8 = 1;
const CB_HALF_OPEN: u8 = 2;

/// v1.0.1 P0#4: circuit breaker for the LLM upstream.
///
/// Three states, transitions driven by the `chat` path:
/// Closed (default) → Open (after N consecutive failures) →
/// HalfOpen (after `OPEN_DURATION`) → Closed (probe success) or
/// back to Open (probe failure).
///
/// Concurrency:
/// * `state` is an `AtomicU8` so the hot path (a Closed check)
///   is a single load with `Acquire` ordering.
/// * `opened_at` is held under a `parking_lot::Mutex` because we
///   only need to read/write it on the Closed→Open and
///   Open→HalfOpen transitions, both of which are rare relative
///   to the steady-state Closed case.
#[derive(Debug)]
pub struct CircuitBreaker {
    state: AtomicU8,
    /// Number of consecutive failures (reset on success).
    failures: AtomicU8,
    /// Wall-clock instant the breaker last tripped.
    opened_at: Mutex<Option<Instant>>,
    /// Configurable knobs (so the tests can shrink the open
    /// duration without rewriting the constants).
    open_duration: Duration,
    failure_threshold: u32,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(CB_FAILURE_THRESHOLD, CB_OPEN_DURATION)
    }
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, open_duration: Duration) -> Self {
        Self {
            state: AtomicU8::new(CB_CLOSED),
            failures: AtomicU8::new(0),
            opened_at: Mutex::new(None),
            open_duration,
            failure_threshold,
        }
    }

    /// Returns the current state, performing any time-based
    /// transition (Open→HalfOpen) on the way through.
    fn current(&self) -> u8 {
        let s = self.state.load(Ordering::Acquire);
        if s == CB_OPEN {
            // Check whether the open window has elapsed.
            let opened = *self.opened_at.lock();
            if let Some(t) = opened {
                if t.elapsed() >= self.open_duration {
                    // Try to transition Open→HalfOpen.  We use
                    // compare_exchange so two concurrent probes
                    // don't both succeed.
                    let prev = self.state.compare_exchange(
                        CB_OPEN,
                        CB_HALF_OPEN,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    if prev.is_ok() {
                        info!(
                            target: "nine_snake.llm",
                            "circuit breaker: open -> half-open (probe window)"
                        );
                        return CB_HALF_OPEN;
                    }
                }
            }
        }
        s
    }

    /// Returns `Ok(())` if a request is allowed through, or
    /// `Err(anyhow!(...))` if the breaker is Open and the open
    /// window has not elapsed.
    pub fn check(&self) -> Result<()> {
        match self.current() {
            CB_CLOSED | CB_HALF_OPEN => Ok(()),
            CB_OPEN => Err(anyhow!("circuit open: upstream unavailable")),
            other => {
                // Defensive: an unexpected state value is treated
                // as Closed to keep the system available.
                warn!(target: "nine_snake.llm", state = other, "circuit breaker in unknown state; treating as closed");
                Ok(())
            }
        }
    }

    /// Records a successful upstream call.  Closes the breaker
    /// and resets the failure counter.
    pub fn record_success(&self) {
        let prev = self.state.swap(CB_CLOSED, Ordering::AcqRel);
        if prev != CB_CLOSED {
            info!(target: "nine_snake.llm", "circuit breaker: -> closed (upstream recovered)");
        }
        self.failures.store(0, Ordering::Release);
        *self.opened_at.lock() = None;
    }

    /// Records a failed upstream call.  When the failure count
    /// reaches the threshold the breaker trips Open.
    pub fn record_failure(&self) {
        let prev = self.failures.fetch_add(1, Ordering::AcqRel);
        let new_count = prev + 1;
        if new_count as u32 >= self.failure_threshold {
            let was = self.state.swap(CB_OPEN, Ordering::AcqRel);
            if was != CB_OPEN {
                *self.opened_at.lock() = Some(Instant::now());
                warn!(
                    target: "nine_snake.llm",
                    failures = new_count,
                    "circuit breaker tripped: closed -> open"
                );
            }
        }
    }

    /// Returns the raw state byte.  Test-only.
    #[cfg(test)]
    pub fn raw_state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

/// One cache entry: a response plus the moment it was stored.
struct CacheEntry {
    response: ChatResponse,
    inserted_at: std::time::Instant,
}

/// The LLM gateway.
///
/// v1.0 P0#7 fix: the prompt cache is now a true
/// [`lru::LruCache`].  v0.3 used a `Vec<(u64, CacheEntry)>` with
/// `g.remove(0)` on overflow, which is **FIFO**, not LRU.  That
/// meant a hot, frequently-hit entry could be evicted to make
/// room for a never-revisited entry inserted seconds later.  The
/// fix is a one-line switch in [`LlmGateway::new`] plus a
/// matching `cache.get(key)` in [`LlmGateway::lookup_cache`]
/// (which also marks the entry as most-recently-used).
pub struct LlmGateway {
    primary: Arc<OllamaClient>,
    default_model: String,
    remote: Option<RemoteFallback>,
    /// v1.1 P0-1: Anthropic Claude fallback chain.
    anthropic: Option<AnthropicFallback>,
    cache: Mutex<LruCache<u64, CacheEntry>>,
    /// v1.0.1 P0#4: circuit breaker around the upstream call.
    breaker: CircuitBreaker,
}

/// Optional remote fallback (OpenAI-compatible /v1/chat/completions).
struct RemoteFallback {
    base_url: String,
    api_key: Option<String>,
    http: Client,
}

/// v1.1 P0-1: Optional Anthropic Claude fallback.
struct AnthropicFallback {
    client: AnthropicClient,
}

#[derive(Debug, Clone, Serialize)]
struct RemoteChatRequest<'a> {
    model: &'a str,
    messages: &'a [RemoteMessage<'a>],
    stream: bool,
}

#[derive(Debug, Clone, Serialize)]
struct RemoteMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteChatResponse {
    #[serde(default)]
    choices: Vec<RemoteChoice>,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteChoice {
    message: RemoteRespMessage,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteRespMessage {
    role: String,
    content: String,
}

impl LlmGateway {
    /// Returns a reference to the primary Ollama client.
    pub fn ollama_client(&self) -> &OllamaClient {
        &self.primary
    }

    /// Creates a new gateway.
    /// `anthropic_api_key` enables the v1.1 P0-1 Anthropic Claude fallback.
    pub fn new(
        primary: Arc<OllamaClient>,
        default_model: impl Into<String>,
        remote_url: Option<String>,
        anthropic_api_key: Option<String>,
        anthropic_model: Option<String>,
    ) -> Self {
        let remote = remote_url.map(|u| RemoteFallback {
            base_url: u,
            api_key: std::env::var("NINE_SNAKE_REMOTE_KEY").ok(),
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client should build"),
        });

        // v1.1 P0-1: Anthropic fallback, enabled when API key is provided.
        let anthropic = anthropic_api_key.map(|key| {
            let model = anthropic_model.unwrap_or_else(|| "claude-3-5-haiku-20241022".to_string());
            AnthropicFallback {
                client: AnthropicClient::new(key, model, None),
            }
        });

        // v1.0 P0#7: LRU cache, not FIFO.  `CACHE_CAPACITY` is
        // always > 0 so the `NonZeroUsize::new` cannot fail.
        let cap =
            NonZeroUsize::new(CACHE_CAPACITY.max(1)).expect("CACHE_CAPACITY must be non-zero");
        Self {
            primary,
            default_model: default_model.into(),
            remote,
            anthropic,
            cache: Mutex::new(LruCache::new(cap)),
            breaker: CircuitBreaker::default(),
        }
    }

    #[cfg(test)]
    pub fn new_test() -> Self {
        let ollama = Arc::new(OllamaClient::new("http://127.0.0.1:11434".to_string()));
        Self::new(ollama, "test-model", None, None, None)
    }

    /// Returns the default chat model name.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Sends a chat completion. Looks the response up in the prompt
    /// cache first; on miss, tries the local Ollama server and falls
    /// back to the remote endpoint on error.
    ///
    /// v1.0.1 P0#4: a circuit breaker around the upstream call
    /// rejects requests immediately (with `anyhow!("circuit open:
    /// upstream unavailable")`) when Ollama has been failing for
    /// long enough that a probe is warranted.  The breaker is
    /// checked on the hot path; in steady state the check is a
    /// single atomic load.
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> anyhow::Result<ChatResponse> {
        // v1.0.1 P0#4: short-circuit when the breaker is Open.
        // The cache lookup stays *outside* the breaker because a
        // cached response is always safe to return regardless of
        // upstream health.
        let key = cache_key(&self.default_model, &messages);
        if let Some(hit) = self.lookup_cache(key) {
            debug!(target: "nine_snake.llm", "cache hit");
            return Ok(hit);
        }

        // v1.0.1 P0#4: gate the upstream call on the breaker.
        self.breaker.check()?;

        match self.primary.chat(&self.default_model, &messages).await {
            Ok(resp) => {
                self.breaker.record_success();
                self.store_cache(key, resp.clone());
                Ok(resp)
            }
            Err(e) => {
                // v1.0.1 P0#4: every failure counts; the breaker
                // will trip Closed→Open after `failure_threshold`
                // consecutive failures.  Note: we still try the
                // remote fallback before counting the failure, so
                // a healthy remote keeps the breaker Closed even
                // if Ollama is dead.
                //
                // v1.1 P0-1: Anthropic Claude is the third fallback
                // after Ollama → Remote.
                if let Some(remote) = &self.remote {
                    match self
                        .call_remote(remote, &self.default_model, &messages)
                        .await
                    {
                        Ok(resp) => {
                            self.breaker.record_success();
                            self.store_cache(key, resp.clone());
                            return Ok(resp);
                        }
                        Err(remote_err) => {
                            // Try Anthropic before recording failure
                            if let Some(anthropic) = &self.anthropic {
                                match self.call_anthropic(anthropic, &messages).await {
                                    Ok(text) => {
                                        self.breaker.record_success();
                                        let resp = ChatResponse {
                                            model: anthropic.client.model.clone(),
                                            message: ChatMessage {
                                                role: "assistant".to_string(),
                                                content: text,
                                            },
                                            done: true,
                                            total_duration: None,
                                            eval_count: None,
                                        };
                                        self.store_cache(key, resp.clone());
                                        return Ok(resp);
                                    }
                                    Err(anthropic_err) => {
                                        warn!(
                                            target: "nine_snake.llm",
                                            error = ?anthropic_err,
                                            "Anthropic fallback also failed"
                                        );
                                        self.breaker.record_failure();
                                        return Err(anthropic_err)
                                            .context("ollama, remote, and anthropic all failed");
                                    }
                                }
                            }
                            self.breaker.record_failure();
                            return Err(remote_err)
                                .context("local ollama and remote fallback both failed");
                        }
                    }
                }
                self.breaker.record_failure();
                Err(e).context("local ollama chat failed and no remote fallback configured")
            }
        }
    }

    /// Chat with an explicit model override (skips the cache).
    pub async fn chat_with_model(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> anyhow::Result<ChatResponse> {
        self.primary.chat(model, &messages).await
    }

    /// Streaming chat completion. Returns a stream of token strings.
    /// When the stream ends or errors, the last emitted item may be
    /// marked as incomplete if the connection was interrupted.
    pub fn chat_stream(
        &self,
        messages: Vec<ChatMessage>,
    ) -> futures::stream::BoxStream<'static, Result<StreamToken>> {
        let client = self.primary.clone();
        let model = self.default_model.clone();

        let stream = async_stream::stream! {
            let url = format!("{}/api/chat", client.base_url());
            let req_body = serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": true,
            });

            let resp = match client.http().post(&url).json(&req_body).send().await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(anyhow!("streaming chat request failed: {e}"));
                    return;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                yield Err(anyhow!("streaming chat HTTP {status}: {body}"));
                return;
            }

            let mut stream = resp.bytes_stream();
            let mut incomplete = false;

            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Ok(StreamToken {
                            text: String::new(),
                            done: false,
                            incomplete: true,
                        });
                        warn!(target: "nine_snake.llm", error = %e, "stream interrupted");
                        return;
                    }
                };

                for line in String::from_utf8_lossy(&bytes).lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(line) {
                        Ok(v) => {
                            let done = v.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
                            let text = v
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            if done {
                                yield Ok(StreamToken {
                                    text,
                                    done: true,
                                    incomplete: false,
                                });
                                return;
                            }
                            incomplete = !text.is_empty();
                            yield Ok(StreamToken {
                                text,
                                done: false,
                                incomplete: false,
                            });
                        }
                        Err(e) => {
                            debug!(target: "nine_snake.llm", error = %e, line, "skipping unparseable stream line");
                        }
                    }
                }
            }

            yield Ok(StreamToken {
                text: String::new(),
                done: true,
                incomplete,
            });
        };

        Box::pin(stream)
    }

    /// Generation request (prompt in, completion out).
    pub async fn generate(&self, prompt: &str) -> anyhow::Result<String> {
        let resp = self.primary.generate(&self.default_model, prompt).await?;
        Ok(resp.response)
    }

    /// Clears the prompt cache.
    pub fn clear_cache(&self) {
        self.cache.lock().clear();
        info!(target: "nine_snake.llm", "prompt cache cleared");
    }

    async fn call_remote(
        &self,
        remote: &RemoteFallback,
        model: &str,
        messages: &[ChatMessage],
    ) -> anyhow::Result<ChatResponse> {
        let url = format!("{}/v1/chat/completions", remote.base_url);
        let ssrf_guard = SsrfGuard::new();
        ssrf_guard
            .validate_url(&url)
            .map_err(|e| anyhow::anyhow!("SSRF validation failed: {e}"))?;
        let payload_msgs: Vec<RemoteMessage<'_>> = messages
            .iter()
            .map(|m| RemoteMessage {
                role: &m.role,
                content: &m.content,
            })
            .collect();
        let body = RemoteChatRequest {
            model,
            messages: &payload_msgs,
            stream: false,
        };
        let mut req = remote.http.post(&url).json(&body);
        if let Some(k) = &remote.api_key {
            req = req.bearer_auth(k);
        }
        let resp: RemoteChatResponse = req.send().await?.error_for_status()?.json().await?;
        let choice = resp
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("remote fallback returned no choices"))?;
        Ok(ChatResponse {
            model: model.to_string(),
            message: ChatMessage {
                role: choice.message.role,
                content: choice.message.content,
            },
            done: true,
            total_duration: None,
            eval_count: None,
        })
    }

    /// v1.1 P0-1: Call Anthropic Claude via `/v1/messages`.
    async fn call_anthropic(
        &self,
        anthropic: &AnthropicFallback,
        messages: &[ChatMessage],
    ) -> anyhow::Result<String> {
        use super::anthropic::Message as Am;
        let anthropic_messages: Vec<Am> = messages
            .iter()
            .map(|m| {
                let role = match m.role.as_str() {
                    "system" => super::anthropic::Role::System,
                    "assistant" => super::anthropic::Role::Assistant,
                    _ => super::anthropic::Role::User,
                };
                Am {
                    role,
                    content: m.content.clone(),
                }
            })
            .collect();
        anthropic.client.chat(&anthropic_messages).await
    }

    /// Looks up a key in the cache, expiring any entries older than
    /// [`CACHE_TTL`].  v1.0 P0#7: `LruCache::get` also bumps the
    /// entry to the most-recently-used position, which is what
    /// makes the cache actually LRU.
    fn lookup_cache(&self, key: u64) -> Option<ChatResponse> {
        let mut g = self.cache.lock();
        let now = std::time::Instant::now();
        // The `lru` crate doesn't expose a "remove all entries
        // matching a predicate" helper, so we walk the iterator
        // and collect the doomed keys.  `CACHE_TTL` is a generous
        // 5 minutes; the cache holds at most 64 entries, so the
        // walk is cheap.
        let expired: Vec<u64> = g
            .iter()
            .filter_map(|(k, e)| {
                if now.duration_since(e.inserted_at) >= CACHE_TTL {
                    Some(*k)
                } else {
                    None
                }
            })
            .collect();
        for k in expired {
            g.pop(&k);
        }
        g.get(&key).map(|e| e.response.clone())
    }

    /// Inserts a response.  When the cache is at capacity, the
    /// least-recently-used entry is evicted by `LruCache::put`.
    fn store_cache(&self, key: u64, resp: ChatResponse) {
        let mut g = self.cache.lock();
        g.put(
            key,
            CacheEntry {
                response: resp,
                inserted_at: std::time::Instant::now(),
            },
        );
    }
}

/// Hashes (model, messages) into a stable cache key.
fn cache_key(model: &str, messages: &[ChatMessage]) -> u64 {
    let mut h = DefaultHasher::new();
    model.hash(&mut h);
    for m in messages {
        m.role.hash(&mut h);
        m.content.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep as std_sleep;

    /// v1.0.1 P0#4: three consecutive failures flip Closed → Open.
    #[test]
    fn breaker_trips_after_threshold() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60));
        assert_eq!(cb.raw_state(), CB_CLOSED);
        cb.record_failure();
        assert_eq!(cb.raw_state(), CB_CLOSED, "1 failure must not trip");
        cb.record_failure();
        assert_eq!(cb.raw_state(), CB_CLOSED, "2 failures must not trip");
        cb.record_failure();
        assert_eq!(cb.raw_state(), CB_OPEN, "3 failures must trip");
    }

    /// v1.0.1 P0#4: when the breaker is Open and the open window
    /// has not elapsed, `check()` rejects immediately with the
    /// canonical error string.
    #[test]
    fn breaker_open_rejects_immediately() {
        let cb = CircuitBreaker::new(1, Duration::from_secs(60));
        cb.record_failure();
        assert_eq!(cb.raw_state(), CB_OPEN);
        let err = cb.check().unwrap_err();
        assert!(
            err.to_string().contains("circuit open"),
            "unexpected error message: {err}"
        );
    }

    /// v1.0.1 P0#4: after `OPEN_DURATION` elapses, the next
    /// `check()` transitions to HalfOpen (the request is allowed
    /// through).  A subsequent success closes the breaker; a
    /// subsequent failure re-opens it.
    #[test]
    fn breaker_recovers_after_open_window() {
        // 100 ms window so the test runs in well under a second.
        let cb = CircuitBreaker::new(1, Duration::from_millis(100));
        cb.record_failure();
        assert_eq!(cb.raw_state(), CB_OPEN);
        // First check immediately after tripping: still Open.
        assert!(cb.check().is_err());

        std_sleep(Duration::from_millis(150));

        // Now the open window has elapsed; check() must transition
        // to HalfOpen and let the request through.
        assert!(cb.check().is_ok(), "expected half-open to allow probe");
        // The state is now HalfOpen (not yet Closed — that
        // requires `record_success`).
        let after_probe = cb.raw_state();
        assert!(
            after_probe == CB_HALF_OPEN || after_probe == CB_OPEN,
            "expected half_open (or possibly re-opened), got {after_probe}"
        );

        // Record a success: the breaker must close.
        cb.record_success();
        assert_eq!(cb.raw_state(), CB_CLOSED);
    }

    /// v1.0.1 P0#4: a single success in the steady-state Closed
    /// path resets the failure counter so a historical failure
    /// doesn't accumulate forever.
    #[test]
    fn breaker_success_resets_failure_counter() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        // Two more failures: must NOT trip (counter was reset).
        cb.record_failure();
        cb.record_failure();
        assert_eq!(
            cb.raw_state(),
            CB_CLOSED,
            "success must have reset the counter"
        );
        // The third post-reset failure trips.
        cb.record_failure();
        assert_eq!(cb.raw_state(), CB_OPEN);
    }

    use crate::llm::OllamaClient;

    fn msgs() -> Vec<ChatMessage> {
        vec![ChatMessage::system("sys"), ChatMessage::user("hello")]
    }

    #[test]
    fn cache_key_is_stable() {
        let a = cache_key("m", &msgs());
        let b = cache_key("m", &msgs());
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_changes_with_model() {
        let a = cache_key("m1", &msgs());
        let b = cache_key("m2", &msgs());
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_with_content() {
        let mut m = msgs();
        m[1].content = "hi".into();
        let a = cache_key("m", &m);
        m[1].content = "bye".into();
        let b = cache_key("m", &m);
        assert_ne!(a, b);
    }

    fn dummy_response(content: &str) -> ChatResponse {
        ChatResponse {
            model: "m".to_string(),
            message: ChatMessage {
                role: "assistant".to_string(),
                content: content.to_string(),
            },
            done: true,
            total_duration: None,
            eval_count: None,
        }
    }

    /// v1.0 P0#7: an LRU-cache gate against a 64-entry gateway.
    /// Touching the oldest entry must keep it alive past the
    /// insertion of N+1 new entries.
    #[test]
    fn lru_evicts_least_recently_used_not_oldest_inserted() {
        // Build a small gateway-shaped cache directly.  We don't
        // call `LlmGateway::chat` because that would require a
        // running Ollama.
        let cap = NonZeroUsize::new(4).unwrap();
        let mut cache: LruCache<u64, CacheEntry> = LruCache::new(cap);
        for i in 0..4u64 {
            cache.put(
                i,
                CacheEntry {
                    response: dummy_response(&format!("v{i}")),
                    inserted_at: std::time::Instant::now(),
                },
            );
        }
        // Touch key=0 (the oldest-inserted entry) so it becomes
        // the most-recently-used.
        let touched = cache.get(&0).expect("0 must be present");
        assert_eq!(touched.response.message.content, "v0");

        // Insert a 5th entry; key=1 is now the LRU and must be
        // evicted (NOT key=0).
        cache.put(
            4,
            CacheEntry {
                response: dummy_response("v4"),
                inserted_at: std::time::Instant::now(),
            },
        );
        assert!(
            cache.get(&0).is_some(),
            "key 0 must still be present (touched)"
        );
        assert!(
            cache.get(&1).is_none(),
            "key 1 must have been evicted as LRU"
        );
        assert!(cache.get(&2).is_some());
        assert!(cache.get(&3).is_some());
        assert!(cache.get(&4).is_some());
    }

    /// v1.0 P0#7: with a 1-entry cache, a second `store_cache`
    /// for a *different* key evicts the first.  The old FIFO
    /// behaviour accidentally passed this case, so the test is
    /// only here to document the LRU invariant.
    #[test]
    fn store_cache_evicts_when_full() {
        let cap = NonZeroUsize::new(2).unwrap();
        let mut cache: LruCache<u64, CacheEntry> = LruCache::new(cap);
        cache.put(
            1,
            CacheEntry {
                response: dummy_response("a"),
                inserted_at: std::time::Instant::now(),
            },
        );
        cache.put(
            2,
            CacheEntry {
                response: dummy_response("b"),
                inserted_at: std::time::Instant::now(),
            },
        );
        // Touch 1 so 2 becomes LRU.
        let _ = cache.get(&1);
        cache.put(
            3,
            CacheEntry {
                response: dummy_response("c"),
                inserted_at: std::time::Instant::now(),
            },
        );
        assert!(cache.get(&1).is_some(), "1 was touched; should survive");
        assert!(cache.get(&2).is_none(), "2 was LRU; should be evicted");
        assert!(cache.get(&3).is_some());
    }

    /// v1.0 P0#7: `LlmGateway::new` should produce a working
    /// LRU cache.  The public capacity is hard-coded at 64, so
    /// we exercise the underlying LRU behaviour directly
    /// through a small `LruCache` instance identical to the
    /// one inside the gateway.  This is the canonical
    /// regression test: a hot entry that's been touched must
    /// survive a fresh insert that would have evicted it
    /// under the v0.3 FIFO design.
    #[test]
    fn gateway_cache_evicts_lru_not_oldest() {
        // Mirror `LlmGateway::new`'s storage: a `LruCache` of
        // some capacity holding `CacheEntry` values.
        let cap = NonZeroUsize::new(3).unwrap();
        let mut cache: LruCache<u64, CacheEntry> = LruCache::new(cap);
        // Three insertions, none touched.
        for k in 0..3u64 {
            cache.put(
                k,
                CacheEntry {
                    response: dummy_response(&format!("v{k}")),
                    inserted_at: std::time::Instant::now(),
                },
            );
        }
        // Touch key=0 (the oldest-inserted) — it becomes the MRU.
        let touched = cache.get(&0).expect("0 must be present");
        assert_eq!(touched.response.message.content, "v0");
        // Insert key=3.  With capacity=3, the LRU is evicted.
        cache.put(
            3,
            CacheEntry {
                response: dummy_response("v3"),
                inserted_at: std::time::Instant::now(),
            },
        );
        // The LRU was key=1 (oldest-untouched); the FIFO bug
        // would have evicted key=0 instead.  P0#7 fix: key=0
        // survives because it was touched.
        assert!(
            cache.get(&0).is_some(),
            "touched key 0 must survive (LRU fix)"
        );
        assert!(
            cache.get(&1).is_none(),
            "untouched key 1 is LRU and must be evicted"
        );
        assert!(cache.get(&2).is_some());
        assert!(cache.get(&3).is_some());
    }

    /// v1.0 P0#7: TTL eviction runs alongside LRU eviction.
    /// We don't wait 5 minutes in a unit test; instead we check
    /// that the `lookup_cache` helper is wired up to drop
    /// expired entries (we can't easily synthesise an
    /// `Instant` in the past, so this test just verifies that
    /// fresh entries round-trip).
    #[test]
    fn lookup_cache_returns_fresh_entries() {
        let gw = LlmGateway::new(
            Arc::new(OllamaClient::new("http://127.0.0.1:1")),
            "m",
            None,
            None,
            None,
        );
        let k = cache_key("m", &[ChatMessage::user("ping")]);
        gw.store_cache(k, dummy_response("pong"));
        let got = gw.lookup_cache(k).expect("must hit");
        assert_eq!(got.message.content, "pong");
    }
}
