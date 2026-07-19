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
use super::{
    ProxyTermination, RequestKey, SseProxyInstance, SseProxyRestructure, SseRestructureState,
    SseTerminalInfo,
};

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
    let mut collected = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(error) => {
            instance.discard(key);
            return Ok(error_response(
                500,
                &format!("sse_proxy body read failed: {error}"),
            ));
        }
    };

    let restructure = instance.provider_config.restructure();
    let mut request_body_restructured = false;
    if let Some(restructure) = restructure {
        if let Some(bytes) = restructure.rewrite_request_body(&collected) {
            collected = bytes;
            request_body_restructured = true;
        }
    }

    let model = extract_model_from_body(&collected);

    tracing::info!(
        provider = %provider,
        model = %model.as_deref().unwrap_or(""),
        method = %parts.method,
        path_and_query = %parts.uri,
        body_bytes = collected.len(),
        request_body_restructured,
        restructure = ?restructure,
        extra_header_count = instance.extra_headers.len(),
        extra_headers = ?instance.extra_headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
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
    for (name, value) in &instance.extra_headers {
        upstream_req = upstream_req.header(name.as_str(), value.as_str());
    }

    tracing::debug!(
        provider = %provider,
        model = %model.as_deref().unwrap_or(""),
        method = %parts.method,
        upstream_url = %upstream_uri,
        local_path_and_query = %parts.uri,
        body_bytes = collected.len(),
        request_body_restructured,
        restructure = ?restructure,
        extra_header_count = instance.extra_headers.len(),
        extra_headers = ?instance.extra_headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
        request_key = key,
        "sse_proxy starting upstream stream request"
    );

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
        tracing::warn!(
            provider = %provider,
            model = %model.as_deref().unwrap_or(""),
            upstream_url = %upstream_uri,
            status,
            response_body_preview = %preview,
            request_key = key,
            "sse_proxy upstream returned non-2xx response"
        );
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

    let upstream_content_type = response_content_type(&response);
    tracing::debug!(
        provider = %provider,
        model = %model.as_deref().unwrap_or(""),
        upstream_url = %upstream_uri,
        status,
        content_type = %upstream_content_type.as_deref().unwrap_or(""),
        request_key = key,
        "sse_proxy upstream response received"
    );

    let is_sse = restructure.is_some_and(|restructure| restructure.force_sse())
        || upstream_content_type
            .as_deref()
            .is_some_and(|content_type| content_type.contains("text/event-stream"));
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
        let preview = String::from_utf8_lossy(&bytes)
            .chars()
            .take(2048)
            .collect::<String>();
        tracing::debug!(
            provider = %provider,
            model = %model.as_deref().unwrap_or(""),
            upstream_url = %upstream_uri,
            status,
            content_type = %upstream_content_type.as_deref().unwrap_or(""),
            response_body_preview = %preview,
            request_key = key,
            "sse_proxy upstream returned non-sse 2xx response"
        );
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
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_str(
                upstream_content_type
                    .as_deref()
                    .unwrap_or("application/octet-stream"),
            )
            .unwrap_or_else(|_| http::HeaderValue::from_static("application/octet-stream")),
        );
        return Ok(response);
    }

    // Streaming SSE path. Spawn a relay task that pulls bytes from the
    // upstream body, parses SSE frames for telemetry, and forwards chunks to
    // the SDK. Some providers need SSE frames reconstructed so terminal events
    // match the downstream SDK's expected shape.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, hyper::Error>>(8);
    let relay_provider = provider.clone();
    let relay_model = model.clone().unwrap_or_default();
    let relay_instance = instance.clone();
    let relay_key = key;
    let relay_started = started;
    let relay_restructure = restructure;
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
        let mut event_count: u64 = 0;
        let mut restructure_state = SseRestructureState::default();

        while let Some(frame) = upstream_stream.next().await {
            match frame {
                Ok(bytes) => {
                    if first_data_at.is_none() && !bytes.is_empty() {
                        first_data_at = Some(elapsed_ms(relay_started));
                    }
                    let events = parser.feed(&bytes);
                    let mut relay_send_failed = false;
                    for event in events {
                        match event {
                            SseEvent::Data(mut payload) => {
                                event_count += 1;
                                if first_data_at.is_none() {
                                    first_data_at = Some(elapsed_ms(relay_started));
                                }
                                let event_type = extract_event_type(&payload);
                                if let Some(restructure) = relay_restructure {
                                    payload = restructure
                                        .restructure_sse_payload(&payload, &mut restructure_state);
                                }
                                let should_log_payload = event_count <= 8
                                    || event_type.as_deref().is_some_and(|event_type| {
                                        event_type.contains("error")
                                            || event_type.contains("incomplete")
                                            || event_type.contains("completed")
                                            || event_type.ends_with(".done")
                                    });
                                if should_log_payload {
                                    tracing::debug!(
                                        provider = %relay_provider,
                                        model = %relay_model,
                                        request_key = relay_key,
                                        event_count,
                                        event_type = %event_type.as_deref().unwrap_or(""),
                                        payload_preview = %payload.chars().take(1024).collect::<String>(),
                                        "sse_proxy upstream sse event"
                                    );
                                }
                                if let Some(event_type) = event_type
                                    .as_deref()
                                    .filter(|event_type| is_responses_terminal_event(event_type))
                                {
                                    terminal_payload = Some(payload.clone());
                                    terminal_at = Some(elapsed_ms(relay_started));
                                    termination = Some(ProxyTermination::ResponsesEvent(
                                        event_type.to_string(),
                                    ));
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
                                if relay_restructure
                                    .is_some_and(|restructure| restructure.reconstruct_sse())
                                    && tx
                                        .send(Ok(Frame::data(sse_data_frame(&payload))))
                                        .await
                                        .is_err()
                                {
                                    relay_send_failed = true;
                                    break;
                                }
                            }
                            SseEvent::Done => {
                                if terminal_at.is_none() {
                                    terminal_at = Some(elapsed_ms(relay_started));
                                }
                                if termination.is_none() {
                                    termination = Some(ProxyTermination::SseDone);
                                }
                                if relay_restructure
                                    .is_some_and(|restructure| restructure.reconstruct_sse())
                                    && tx.send(Ok(Frame::data(sse_done_frame()))).await.is_err()
                                {
                                    relay_send_failed = true;
                                    break;
                                }
                            }
                        }
                    }
                    if relay_send_failed {
                        break;
                    }
                    if !relay_restructure.is_some_and(|restructure| restructure.reconstruct_sse())
                        && tx.send(Ok(Frame::data(bytes))).await.is_err()
                    {
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
            match event {
                SseEvent::Data(mut payload) => {
                    if let Some(restructure) = relay_restructure {
                        payload =
                            restructure.restructure_sse_payload(&payload, &mut restructure_state);
                    }
                    if relay_restructure.is_some_and(|restructure| restructure.reconstruct_sse()) {
                        let _ = tx.send(Ok(Frame::data(sse_data_frame(&payload)))).await;
                    }
                }
                SseEvent::Done => {
                    if terminal_at.is_none() {
                        terminal_at = Some(elapsed_ms(relay_started));
                    }
                    if termination.is_none() {
                        termination = Some(ProxyTermination::SseDone);
                    }
                    if relay_restructure.is_some_and(|restructure| restructure.reconstruct_sse()) {
                        let _ = tx.send(Ok(Frame::data(sse_done_frame()))).await;
                    }
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
            completed_output_injected = restructure_state.completed_output_injected,
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

fn response_content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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

fn extract_event_type(payload: &str) -> Option<String> {
    let value: Value = serde_json::from_str(payload).ok()?;
    value
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn is_responses_terminal_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.completed" | "response.incomplete" | "error"
    )
}

impl SseProxyRestructure {
    fn rewrite_request_body(self, body: &Bytes) -> Option<Bytes> {
        match self {
            SseProxyRestructure::OpenAiCodexResponses => {
                let mut body_json: Value = serde_json::from_slice(body).ok()?;
                let object = body_json.as_object_mut()?;
                object.insert("store".to_string(), Value::Bool(false));
                serde_json::to_vec(&body_json).ok().map(Bytes::from)
            }
        }
    }

    fn force_sse(self) -> bool {
        match self {
            SseProxyRestructure::OpenAiCodexResponses => true,
        }
    }

    fn reconstruct_sse(self) -> bool {
        match self {
            SseProxyRestructure::OpenAiCodexResponses => true,
        }
    }

    fn restructure_sse_payload(self, payload: &str, state: &mut SseRestructureState) -> String {
        match self {
            SseProxyRestructure::OpenAiCodexResponses => {
                if extract_event_type(payload).as_deref() == Some("response.output_item.done") {
                    state.last_responses_output_item = extract_responses_output_item(payload);
                    return payload.to_string();
                }
                if extract_event_type(payload).as_deref() == Some("response.completed") {
                    if let Some(rewritten) = inject_responses_output_if_empty(
                        payload,
                        state.last_responses_output_item.as_ref(),
                    ) {
                        state.completed_output_injected = true;
                        return rewritten;
                    }
                }
                payload.to_string()
            }
        }
    }
}

fn extract_responses_output_item(payload: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("response.output_item.done") {
        return None;
    }
    value.get("item").cloned()
}

fn inject_responses_output_if_empty(payload: &str, output_item: Option<&Value>) -> Option<String> {
    let mut output_item = output_item?.clone();
    remove_specific_data_bytes(&mut output_item);
    let mut value: Value = serde_json::from_str(payload).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("response.completed") {
        return None;
    }

    let response = value.get_mut("response")?.as_object_mut()?;
    match response.get("output") {
        Some(Value::Array(output)) if output.is_empty() => {}
        None => {}
        _ => return None,
    }
    response.insert("output".to_string(), Value::Array(vec![output_item]));
    serde_json::to_string(&value).ok()
}

fn remove_specific_data_bytes(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(specific_data)) = map.get_mut("specific_data") {
                specific_data.remove("bytes");
            }
            for child in map.values_mut() {
                remove_specific_data_bytes(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                remove_specific_data_bytes(item);
            }
        }
        _ => {}
    }
}

fn sse_data_frame(payload: &str) -> Bytes {
    let mut frame = String::new();
    for line in payload.split('\n') {
        frame.push_str("data: ");
        frame.push_str(line);
        frame.push('\n');
    }
    frame.push('\n');
    Bytes::from(frame)
}

fn sse_done_frame() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn extract_text_delta_size(payload: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("type").and_then(Value::as_str) == Some("response.output_text.delta") {
        return value
            .get("delta")
            .and_then(Value::as_str)
            .map(str::len)
            .filter(|len| *len > 0);
    }
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
    // The SSE proxy's outbound goes to a provider's hardcoded
    // HTTPS endpoint (api.openai.com, api.anthropic.com, etc.)
    // configured by the project, NOT to a model-chosen URL.
    // SSRF guard doesn't apply here.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_output_item_into_empty_responses_completed_output() {
        let output_item_payload = r#"{"type":"response.output_item.done","item":{"id":"msg_1","type":"message","status":"completed","content":[{"type":"output_text","text":"hello"}],"role":"assistant"}}"#;
        let completed_payload =
            r#"{"type":"response.completed","response":{"id":"resp_1","output":[]}}"#;

        let item = extract_responses_output_item(output_item_payload).expect("output item");
        let rewritten = inject_responses_output_if_empty(completed_payload, Some(&item))
            .expect("rewritten completion");
        let value: Value = serde_json::from_str(&rewritten).expect("valid json");

        let output = value
            .get("response")
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array)
            .expect("response output array");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].get("id").and_then(Value::as_str), Some("msg_1"));
    }

    #[test]
    fn does_not_replace_non_empty_responses_completed_output() {
        let item = serde_json::json!({"id":"msg_2","type":"message"});
        let completed_payload = r#"{"type":"response.completed","response":{"id":"resp_1","output":[{"id":"existing"}]}}"#;

        assert!(inject_responses_output_if_empty(completed_payload, Some(&item)).is_none());
    }

    #[test]
    fn removes_bytes_from_nested_specific_data_before_serializing_output_item() {
        let item = serde_json::json!({
            "id": "msg_3",
            "type": "message",
            "content": [
                {
                    "type": "output_text",
                    "text": "hello",
                    "specific_data": {
                        "bytes": [1, 2, 3],
                        "mime_type": "text/plain"
                    }
                }
            ],
            "specific_data": {
                "bytes": "abc",
                "other": true
            }
        });
        let completed_payload =
            r#"{"type":"response.completed","response":{"id":"resp_1","output":[]}}"#;

        let rewritten = inject_responses_output_if_empty(completed_payload, Some(&item))
            .expect("rewritten completion");
        let value: Value = serde_json::from_str(&rewritten).expect("valid json");
        let output_item = value
            .get("response")
            .and_then(|response| response.get("output"))
            .and_then(Value::as_array)
            .and_then(|output| output.first())
            .expect("output item");

        let top_specific_data = output_item
            .get("specific_data")
            .and_then(Value::as_object)
            .expect("top-level specific data");
        assert!(!top_specific_data.contains_key("bytes"));
        assert_eq!(
            top_specific_data.get("other").and_then(Value::as_bool),
            Some(true)
        );

        let nested_specific_data = output_item
            .get("content")
            .and_then(Value::as_array)
            .and_then(|content| content.first())
            .and_then(|content| content.get("specific_data"))
            .and_then(Value::as_object)
            .expect("nested specific data");
        assert!(!nested_specific_data.contains_key("bytes"));
        assert_eq!(
            nested_specific_data
                .get("mime_type")
                .and_then(Value::as_str),
            Some("text/plain")
        );
    }
}
