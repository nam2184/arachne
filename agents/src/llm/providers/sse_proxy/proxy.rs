//! Loopback HTTP server that forwards every request to a fixed upstream
//! base URL, captures SSE `data:` frames, and stores terminal metadata
//! per in-flight request. Used by [`crate::llm::providers::sse_proxy`].
//!
//! Implementation notes:
//! - The loopback side uses hyper 1.x because we control the wire
//!   protocol entirely (loopback, single-tenant, no need for a full
//!   framework).
//! - The upstream side uses `reqwest` (already a dependency with
//!   `rustls-tls`) so HTTPS, redirects, and header canonicalisation are
//!   handled for us. We then drive `reqwest::Response::bytes_stream`
//!   ourselves so we can tee the SSE bytes back to the SDK while
//!   parsing them for telemetry.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_stream::StreamExt as _;

use super::parser::{SseEvent, SseParser};
use super::{ProxyTermination, RequestKey, SseProxyInstance, SseTerminalInfo};

/// Run the proxy forever, accepting one connection at a time. Each
/// connection handles exactly one HTTP request; SSE responses are
/// streamed back via chunked transfer-encoding.
pub async fn serve_loop(
    listener: TcpListener,
    upstream_base_url: String,
    instance: Arc<SseProxyInstance>,
) -> Result<(), String> {
    let provider = instance.provider.clone();
    let upstream = upstream_base_url.clone();
    tracing::info!(
        provider = %provider,
        upstream_base_url = %upstream,
        local_base_url = %instance.local_base_url,
        "sse_proxy started"
    );
    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(
                    provider = %provider,
                    error = %error,
                    "sse_proxy failed to accept connection"
                );
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let upstream_for_conn = upstream.clone();
        let instance_for_conn = instance.clone();
        let service = service_fn(move |req| {
            let upstream = upstream_for_conn.clone();
            let instance = instance_for_conn.clone();
            async move { handle_request(req, upstream, instance).await }
        });
        let provider_for_log = provider.clone();
        tokio::spawn(async move {
            if let Err(error) = hyper::server::conn::http1::Builder::new()
                .preserve_header_case(true)
                .serve_connection(io, service)
                .await
            {
                tracing::warn!(
                    provider = %provider_for_log,
                    peer_addr = %peer_addr,
                    error = %error,
                    "sse_proxy connection error"
                );
            }
        });
    }
}

type ProxyResult = Result<Response<UnsyncBoxBody<Bytes, hyper::Error>>, hyper::Error>;

async fn handle_request(
    req: Request<Incoming>,
    upstream_base_url: String,
    instance: Arc<SseProxyInstance>,
) -> ProxyResult {
    let provider = instance.provider.clone();
    let key = next_request_key();
    let started = Instant::now();

    // Subscribe to terminal info before issuing the upstream call so we
    // never race the SDK finishing before the proxy records metadata.
    let _terminal_rx = instance.register(key);

    let (parts, body) = req.into_parts();
    let collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(error) => {
            instance.discard(key);
            return Ok(error_response(
                500,
                &format!("sse_proxy body read failed: {error}"),
            ));
        }
    };

    let model = extract_model_from_body(&collected);

    tracing::info!(
        provider = %provider,
        model = %model.as_deref().unwrap_or(""),
        method = %parts.method,
        path_and_query = %parts.uri,
        body_bytes = collected.len(),
        request_key = key,
        "sse_proxy received request"
    );

    let upstream_uri = match build_upstream_uri(&upstream_base_url, &parts.uri) {
        Ok(uri) => uri,
        Err(error) => {
            instance.discard(key);
            return Ok(error_response(
                502,
                &format!("sse_proxy invalid upstream uri: {error}"),
            ));
        }
    };

    tracing::debug!(
        provider = %provider,
        model = %model.as_deref().unwrap_or(""),
        method = %parts.method,
        upstream_base_url = %upstream_base_url,
        upstream_url = %upstream_uri,
        request_key = key,
        "sse_proxy formulated upstream url"
    );

    let client = match reqwest_client() {
        Ok(c) => c,
        Err(error) => {
            instance.discard(key);
            return Ok(error_response(500, &error));
        }
    };

    let mut upstream_req = client
        .request(parts.method.clone(), upstream_uri.to_string())
        .body(collected.to_vec());
    for (name, value) in parts.headers.iter() {
        if matches!(
            *name,
            http::header::CONNECTION
                | http::header::HOST
                | http::header::TRANSFER_ENCODING
                | http::header::UPGRADE
                | http::header::CONTENT_LENGTH
        ) {
            continue;
        }
        if let Ok(value_str) = value.to_str() {
            upstream_req = upstream_req.header(name.as_str(), value_str);
        }
    }

    let response_result = upstream_req.send().await;
    let response = match response_result {
        Ok(r) => r,
        Err(error) => {
            let info = Arc::new(SseTerminalInfo {
                termination: Some(ProxyTermination::ByteStreamError(error.to_string())),
                ..terminal_template(&instance, &model)
            });
            instance.finish(key, info);
            return Ok(error_response(
                502,
                &format!("sse_proxy upstream connect failed: {error}"),
            ));
        }
    };

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body_bytes = match response.bytes().await {
            Ok(b) => b,
            Err(error) => {
                instance.discard(key);
                return Ok(error_response(
                    502,
                    &format!("sse_proxy non-2xx body read failed: {error}"),
                ));
            }
        };
        let preview = String::from_utf8_lossy(&body_bytes)
            .chars()
            .take(2048)
            .collect::<String>();
        let info = Arc::new(SseTerminalInfo {
            termination: Some(ProxyTermination::HttpStatus(status)),
            terminal_data_payload: Some(preview.clone()),
            ..terminal_template(&instance, &model)
        });
        instance.finish(key, info);
        let body = Full::new(Bytes::from(body_bytes))
            .map_err(|never| match never {})
            .boxed_unsync();
        let mut response = Response::new(body);
        *response.status_mut() =
            http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_GATEWAY);
        return Ok(response);
    }

    let is_sse = is_sse_response(&response);
    if !is_sse {
        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(error) => {
                instance.discard(key);
                return Ok(error_response(
                    502,
                    &format!("sse_proxy non-stream body read failed: {error}"),
                ));
            }
        };
        let info = Arc::new(SseTerminalInfo {
            termination: Some(ProxyTermination::SseDone),
            terminal_data_payload: Some(String::from_utf8_lossy(&bytes).into_owned()),
            ..terminal_template(&instance, &model)
        });
        instance.finish(key, info);
        let body = Full::new(bytes)
            .map_err(|never| match never {})
            .boxed_unsync();
        let mut response = Response::new(body);
        *response.status_mut() = http::StatusCode::OK;
        return Ok(response);
    }

    // Streaming SSE path. Spawn a relay task that pulls bytes from the
    // upstream body, parses SSE frames for telemetry, and forwards
    // chunks to the SDK unchanged.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, hyper::Error>>(8);
    let relay_provider = provider.clone();
    let relay_model = model.clone().unwrap_or_default();
    let relay_instance = instance.clone();
    let relay_key = key;
    let relay_started = started;
    let mut upstream_stream = response.bytes_stream();
    tokio::spawn(async move {
        let mut parser = SseParser::new();
        let mut text_delta_count: u64 = 0;
        let mut text_byte_count: u64 = 0;
        let mut tool_call_delta_count: u64 = 0;
        let mut first_data_at: Option<u128> = None;
        let mut terminal_at: Option<u128> = None;
        let mut finish_reason_raw: Option<String> = None;
        let mut terminal_payload: Option<String> = None;
        let mut termination: Option<ProxyTermination> = None;

        while let Some(frame) = upstream_stream.next().await {
            match frame {
                Ok(bytes) => {
                    if first_data_at.is_none() && !bytes.is_empty() {
                        first_data_at = Some(elapsed_ms(relay_started));
                    }
                    let events = parser.feed(&bytes);
                    for event in events {
                        match event {
                            SseEvent::Data(payload) => {
                                if first_data_at.is_none() {
                                    first_data_at = Some(elapsed_ms(relay_started));
                                }
                                if let Some(reason) = extract_finish_reason(&payload) {
                                    finish_reason_raw = Some(reason.clone());
                                    terminal_payload = Some(payload.clone());
                                    terminal_at = Some(elapsed_ms(relay_started));
                                    termination = Some(ProxyTermination::FinishReason(reason));
                                }
                                if let Some(count) = extract_text_delta_size(&payload) {
                                    text_delta_count += 1;
                                    text_byte_count += count as u64;
                                }
                                if payload.contains("\"tool_calls\"") {
                                    tool_call_delta_count += 1;
                                }
                            }
                            SseEvent::Done => {
                                if terminal_at.is_none() {
                                    terminal_at = Some(elapsed_ms(relay_started));
                                }
                                if termination.is_none() {
                                    termination = Some(ProxyTermination::SseDone);
                                }
                            }
                        }
                    }
                    if tx.send(Ok(Frame::data(bytes))).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    termination = Some(ProxyTermination::ByteStreamError(error.to_string()));
                    terminal_at = Some(elapsed_ms(relay_started));
                    break;
                }
            }
        }

        let tail = parser.flush();
        for event in tail {
            if let SseEvent::Done = event {
                if terminal_at.is_none() {
                    terminal_at = Some(elapsed_ms(relay_started));
                }
                if termination.is_none() {
                    termination = Some(ProxyTermination::SseDone);
                }
            }
        }

        if termination.is_none() {
            termination = Some(ProxyTermination::EofWithoutFinish);
            terminal_at = Some(elapsed_ms(relay_started));
        }

        let info = Arc::new(SseTerminalInfo {
            provider: relay_provider.clone(),
            model: relay_model,
            finish_reason_raw,
            termination,
            terminal_data_payload: terminal_payload,
            text_delta_count,
            text_byte_count,
            tool_call_delta_count,
            first_data_at_ms: first_data_at,
            terminal_at_ms: terminal_at,
            ..terminal_template(&relay_instance, &None)
        });

        tracing::info!(
            provider = %relay_provider,
            model = %info.model,
            request_key = relay_key,
            finish_reason_raw = ?info.finish_reason_raw,
            text_delta_count = info.text_delta_count,
            text_byte_count = info.text_byte_count,
            tool_call_delta_count = info.tool_call_delta_count,
            first_data_at_ms = ?info.first_data_at_ms,
            terminal_at_ms = ?info.terminal_at_ms,
            duration_ms = ?info.duration_ms(),
            termination = ?info.termination,
            terminal_data_preview = ?info.terminal_data_payload.as_deref().map(|s| s.chars().take(512).collect::<String>()),
            "sse_proxy stream finished"
        );

        relay_instance.finish(relay_key, info);
        drop(tx);
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = http_body_util::StreamBody::new(stream).boxed_unsync();

    let mut response = Response::new(body);
    *response.status_mut() = http::StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("keep-alive"),
    );
    Ok(response)
}

fn terminal_template(instance: &Arc<SseProxyInstance>, model: &Option<String>) -> SseTerminalInfo {
    SseTerminalInfo {
        provider: instance.provider.clone(),
        model: model.clone().unwrap_or_default(),
        upstream_base_url: instance.upstream_base_url.clone(),
        local_base_url: instance.local_base_url.clone(),
        ..Default::default()
    }
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn error_response(status: u16, message: &str) -> Response<UnsyncBoxBody<Bytes, hyper::Error>> {
    let body = Full::new(Bytes::from(message.to_string()))
        .map_err(|never| match never {})
        .boxed_unsync();
    let mut response = Response::new(body);
    *response.status_mut() =
        http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::BAD_GATEWAY);
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn is_sse_response(response: &reqwest::Response) -> bool {
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    content_type.contains("text/event-stream")
}

fn extract_model_from_body(body: &Bytes) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn extract_finish_reason(payload: &str) -> Option<String> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let choices = value.get("choices")?.as_array()?;
    for choice in choices {
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            return Some(reason.to_string());
        }
    }
    None
}

fn extract_text_delta_size(payload: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let choices = value.get("choices")?.as_array()?;
    let mut total = 0usize;
    for choice in choices {
        if let Some(content) = choice
            .get("delta")
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
        {
            total += content.len();
        }
    }
    if total == 0 {
        None
    } else {
        Some(total)
    }
}

fn build_upstream_uri(
    upstream_base_url: &str,
    incoming_uri: &http::Uri,
) -> Result<http::Uri, String> {
    let path_and_query = incoming_uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let base = upstream_base_url.trim_end_matches('/');
    format!("{base}{path_and_query}")
        .parse::<http::Uri>()
        .map_err(|e| format!("upstream uri parse failed: {e}"))
}

fn reqwest_client() -> Result<reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .map_err(|e| format!("sse_proxy failed to build reqwest client: {e}"))
        })
        .clone()
}

static REQUEST_KEY_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_request_key() -> RequestKey {
    REQUEST_KEY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
