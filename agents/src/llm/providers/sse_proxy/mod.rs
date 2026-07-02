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
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use serde_json::Value;
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
    /// Provider emitted a terminal OpenAI Responses API event.
    ResponsesEvent(String),
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

/// Provider-specific stream repair requested by the provider setup. The proxy
/// code calls this through [`SseProxyProviderConfig::restructure`] instead of
/// hard-coding upstream URLs in the forwarding path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseProxyRestructure {
    /// ChatGPT Codex Responses streams omit data AISDK expects on terminal
    /// events and do not set a response Content-Type.
    OpenAiCodexResponses,
}

#[derive(Debug, Clone, Default)]
pub struct SseRestructureState {
    pub last_responses_output_item: Option<Value>,
    pub completed_output_injected: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SseProxyOptions {
    pub extra_headers: Vec<(String, String)>,
    pub require_sse_restructure: bool,
}

/// Provider descriptor carried by each proxy instance. This is intentionally
/// small and explicit: callers decide whether restructure is required, then the
/// module maps provider identity to the concrete restructure implementation.
#[derive(Debug, Clone)]
pub struct SseProxyProviderConfig {
    pub name: String,
    pub require_sse_restructure: bool,
}

impl SseProxyProviderConfig {
    pub fn new(name: String, require_sse_restructure: bool) -> Self {
        Self {
            name,
            require_sse_restructure,
        }
    }

    pub fn restructure(&self) -> Option<SseProxyRestructure> {
        if self.require_sse_restructure && self.name.eq_ignore_ascii_case("openai") {
            Some(SseProxyRestructure::OpenAiCodexResponses)
        } else {
            None
        }
    }
}

/// One proxied upstream. Spawned lazily and shared across requests. Each
/// instance owns its own loopback listener.
pub struct SseProxyInstance {
    pub provider: String,
    pub provider_config: SseProxyProviderConfig,
    pub upstream_base_url: String,
    pub local_base_url: String,
    pub extra_headers: Vec<(String, String)>,
    pending: Mutex<HashMap<RequestKey, broadcast::Sender<Arc<SseTerminalInfo>>>>,
}

impl SseProxyInstance {
    fn new(
        provider_config: SseProxyProviderConfig,
        upstream_base_url: String,
        local_base_url: String,
        options: SseProxyOptions,
    ) -> Self {
        let provider = provider_config.name.clone();
        Self {
            provider,
            provider_config,
            upstream_base_url,
            local_base_url,
            extra_headers: options.extra_headers,
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

static GLOBAL_MANAGER: OnceLock<SseProxyManager> = OnceLock::new();

pub fn global_manager() -> &'static SseProxyManager {
    GLOBAL_MANAGER.get_or_init(SseProxyManager::new)
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
        self.ensure_for_provider_with_options(
            provider,
            upstream_base_url,
            SseProxyOptions::default(),
        )
    }

    /// Like [`SseProxyManager::ensure_for_provider`], but injects the provided
    /// headers into every upstream request. Header values are part of the cache
    /// key because OAuth account affinity can differ between provider configs.
    pub fn ensure_for_provider_with_headers(
        &self,
        provider: &str,
        upstream_base_url: &str,
        extra_headers: &[(String, String)],
    ) -> Result<(String, Arc<SseProxyInstance>), String> {
        self.ensure_for_provider_with_options(
            provider,
            upstream_base_url,
            SseProxyOptions {
                extra_headers: extra_headers.to_vec(),
                require_sse_restructure: false,
            },
        )
    }

    pub fn ensure_for_provider_with_options(
        &self,
        provider: &str,
        upstream_base_url: &str,
        options: SseProxyOptions,
    ) -> Result<(String, Arc<SseProxyInstance>), String> {
        let cache_key = format!(
            "{provider}|{upstream_base_url}|restructure={}|{}",
            options.require_sse_restructure,
            options
                .extra_headers
                .iter()
                .map(|(name, value)| format!("{}={}", name.to_ascii_lowercase(), value))
                .collect::<Vec<_>>()
                .join(";")
        );
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
        let provider_config =
            SseProxyProviderConfig::new(provider_owned.clone(), options.require_sse_restructure);

        let instance = Arc::new(SseProxyInstance::new(
            provider_config,
            upstream_owned.clone(),
            local_base_url.clone(),
            options,
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
