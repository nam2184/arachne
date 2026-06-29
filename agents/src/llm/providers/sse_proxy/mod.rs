//! Loopback SSE proxy used to inspect the raw OpenAI-compatible wire
//! shape for unstable providers.
//!
//! The proxy runs an in-process HTTP server on `127.0.0.1:<random>`. Each
//! per-provider instance forwards every request to a fixed upstream base
//! URL, captures SSE `data:` frames, and exposes terminal metadata so the
//! runner / SDK provider can decide whether to retry or surface a
//! user-visible diagnostic.
//!
//! This is a real product feature, not a debug-only tool: providers like
//! `minimax` are routed through it via either the per-provider
//! `sse_proxy` flag on `ProviderConfig` or the
//! `ARACHNE_SSE_PROXY_PROVIDERS` env override.

pub mod parser;
pub mod proxy;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::broadcast;

pub use parser::SseEvent;

/// What triggered the upstream stream to terminate from the proxy's
/// point of view. The SDK may report a different normalised reason — the
/// proxy captures both for diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyTermination {
    /// Provider sent `data: [DONE]`.
    SseDone,
    /// Provider emitted a JSON chunk with `choices[*].finish_reason` set.
    FinishReason(String),
    /// Upstream response body ended without `[DONE]` or `finish_reason`.
    EofWithoutFinish,
    /// Reqwest stream error (network drop, decode error, etc).
    ByteStreamError(String),
    /// Upstream returned a non-2xx status.
    HttpStatus(u16),
}

/// Structured metadata the proxy captures for one stream. Stored per
/// request key in [`SseProxyManager`] and consumed by the SDK provider
/// once the runner reaches `Finish`.
#[derive(Debug, Clone, Default)]
pub struct SseTerminalInfo {
    pub provider: String,
    pub model: String,
    pub upstream_base_url: String,
    pub local_base_url: String,
    pub finish_reason_raw: Option<String>,
    pub termination: Option<ProxyTermination>,
    pub terminal_data_payload: Option<String>,
    pub text_delta_count: u64,
    pub text_byte_count: u64,
    pub tool_call_delta_count: u64,
    pub first_data_at_ms: Option<u128>,
    pub terminal_at_ms: Option<u128>,
}

impl SseTerminalInfo {
    pub fn new(
        provider: String,
        model: String,
        upstream_base_url: String,
        local_base_url: String,
    ) -> Self {
        Self {
            provider,
            model,
            upstream_base_url,
            local_base_url,
            ..Default::default()
        }
    }

    pub fn duration_ms(&self) -> Option<u128> {
        match (self.first_data_at_ms, self.terminal_at_ms) {
            (Some(start), Some(end)) if end >= start => Some(end - start),
            _ => None,
        }
    }
}

/// Identifier for an in-flight stream. The SDK provider generates this
/// before each `stream()` call and passes it to the manager so the proxy
/// can attach terminal metadata to the correct stream.
pub type RequestKey = u64;

/// One proxied upstream. Spawned lazily and shared across requests. Each
/// instance owns its own loopback listener.
pub struct SseProxyInstance {
    pub provider: String,
    pub upstream_base_url: String,
    pub local_base_url: String,
    pending: Mutex<HashMap<RequestKey, broadcast::Sender<Arc<SseTerminalInfo>>>>,
}

impl SseProxyInstance {
    fn new(provider: String, upstream_base_url: String, local_base_url: String) -> Self {
        Self {
            provider,
            upstream_base_url,
            local_base_url,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, key: RequestKey) -> broadcast::Receiver<Arc<SseTerminalInfo>> {
        let tx = broadcast::channel(1).0;
        self.pending.lock().insert(key, tx.clone());
        tx.subscribe()
    }

    pub fn finish(&self, key: RequestKey, info: Arc<SseTerminalInfo>) {
        if let Some(tx) = self.pending.lock().remove(&key) {
            let _ = tx.send(info);
        }
    }

    pub fn discard(&self, key: RequestKey) {
        self.pending.lock().remove(&key);
    }
}

/// Manager that lazily spawns one [`SseProxyInstance`] per unique upstream
/// base URL. Callers ask for the loopback URL via
/// [`SseProxyManager::ensure_for_provider`].
pub struct SseProxyManager {
    inner: Mutex<HashMap<String, Arc<SseProxyInstance>>>,
}

impl SseProxyManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Idempotently ensure a proxy instance exists for the given
    /// provider/upstream pair. Returns the loopback `base_url` that the
    /// SDK/HTTP provider should be configured with, and the [`SseProxyInstance`]
    /// handle so the caller can subscribe to terminal info per request.
    pub fn ensure_for_provider(
        &self,
        provider: &str,
        upstream_base_url: &str,
    ) -> Result<(String, Arc<SseProxyInstance>), String> {
        let cache_key = format!("{provider}|{upstream_base_url}");
        if let Some(existing) = self.inner.lock().get(&cache_key).cloned() {
            return Ok((existing.local_base_url.clone(), existing));
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("sse_proxy: failed to bind loopback listener: {error}"))?;
        let local_addr = listener
            .local_addr()
            .map_err(|error| format!("sse_proxy: failed to read local_addr: {error}"))?;
        // Drop the std listener — we hand off the bound socket to tokio below.
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("sse_proxy: failed to set listener non-blocking: {error}"))?;
        let std_listener = listener;
        let tokio_listener = tokio::net::TcpListener::from_std(std_listener)
            .map_err(|error| format!("sse_proxy: failed to convert listener to tokio: {error}"))?;

        let provider_owned = provider.to_string();
        let upstream_owned = upstream_base_url.to_string();
        let local_base_url = format!("http://{local_addr}");

        let instance = Arc::new(SseProxyInstance::new(
            provider_owned.clone(),
            upstream_owned.clone(),
            local_base_url.clone(),
        ));

        let instance_for_task = Arc::clone(&instance);
        tokio::spawn(async move {
            if let Err(error) =
                proxy::serve_loop(tokio_listener, upstream_owned, instance_for_task).await
            {
                tracing::error!(
                    provider = %provider_owned,
                    error = %error,
                    "sse_proxy loop terminated unexpectedly"
                );
            }
        });

        self.inner.lock().insert(cache_key, Arc::clone(&instance));
        Ok((local_base_url, instance))
    }

    /// Whether `provider` should be routed through the proxy according to
    /// the `ARACHNE_SSE_PROXY_PROVIDERS` env override. Comma-separated,
    /// case-insensitive provider names.
    pub fn env_enabled(provider: &str) -> bool {
        match std::env::var("ARACHNE_SSE_PROXY_PROVIDERS") {
            Ok(value) => value
                .split(',')
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .any(|segment| segment.eq_ignore_ascii_case(provider)),
            Err(_) => false,
        }
    }
}
