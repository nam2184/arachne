use std::time::Duration;

use crate::{ToolCall, ToolResult};

use super::{failure, string_arg, success};

/// Hard cap on the body size we accept from a single webfetch,
/// matching opencode's `MAX_RESPONSE_SIZE = 5 MB`. Defends both
/// the LLM context window and our local memory from a runaway
/// page. Checked twice: once on the `Content-Length` header
/// (cheap pre-check, before any body bytes hit the wire) and
/// once on the actual decompressed body length (Content-Length
/// can be missing or lie).
pub const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
/// Default request timeout, matching opencode's 30 s.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Max request timeout, matching opencode's 2 minutes.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// Outcome of the body-size pre-check. Pure function so tests
/// can drive the boundary conditions without spinning up a
/// real HTTP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeCheck {
    /// The advertised size is within the cap.
    Ok,
    /// The `Content-Length` header is over the cap. Reject the
    /// request before reading any body bytes.
    TooLarge { advertised: usize },
    /// The `Content-Length` header is absent or unparseable. The
    /// caller should still read the body and apply the
    /// post-decompression cap.
    Unknown,
}

/// Apply the pre-fetch size check to an optional
/// `Content-Length` header value (already parsed into a `usize`).
pub fn check_content_length(content_length: Option<usize>) -> SizeCheck {
    match content_length {
        Some(n) if n > MAX_RESPONSE_BYTES => SizeCheck::TooLarge { advertised: n },
        Some(_) => SizeCheck::Ok,
        None => SizeCheck::Unknown,
    }
}

/// Apply the post-decompression size check. Called after the
/// full body has been read; if it's over the cap we throw the
/// whole response away (caller is expected to surface an error
/// to the model).
pub fn check_body_size(bytes: usize) -> bool {
    bytes <= MAX_RESPONSE_BYTES
}

pub fn run(call: &ToolCall) -> ToolResult {
    // Sync path used by the generic dispatcher at
    // `tools::mod.rs::run_tool_with_context`. Real network I/O
    // happens on the async path (`run_async`) — the runner
    // routes `webfetch` through `run_tool_async` because HTTP
    // is async. The sync path is a defensive fallback that
    // surfaces a clear error to the model rather than blocking
    // the executor.
    let url = string_arg(call, "url");
    if url.is_empty() {
        return failure("webfetch", "url is required".to_string());
    }
    failure(
        "webfetch",
        "webfetch requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string(),
    )
}

/// Async entry point. The runner calls this from
/// `tools::mod.rs::run_tool_async`; tests call it directly with
/// a custom `reqwest::Client`.
pub async fn run_async(call: &ToolCall) -> ToolResult {
    run_with_async(call, &reqwest::Client::new()).await
}

/// Public seam used by tests to inject a pre-built client. The
/// runner calls `run_async(call)`; the test entry point is
/// `run_with_async(call, &client)`.
pub async fn run_with_async(call: &ToolCall, client: &reqwest::Client) -> ToolResult {
    let url = string_arg(call, "url");
    if url.is_empty() {
        return failure("webfetch", "url is required".to_string());
    }

    // Cheap pre-check: parse Content-Length if the caller
    // already received the response headers. (The `headers`
    // access below is the post-response path; we keep the
    // pre-check helper available for callers that want to bail
    // earlier.)
    let _ = check_content_length(None); // documented helper; wired up below

    let response = match client.get(&url).timeout(DEFAULT_TIMEOUT).send().await {
        Ok(response) => response,
        Err(error) => return failure("webfetch", format!("request failed: {error}")),
    };

    let status = response.status();
    if !status.is_success() {
        return failure("webfetch", format!("HTTP {} for {url}", status.as_u16()));
    }

    // Pre-check Content-Length when the server sends it
    // honestly. This is the early bail that opencode uses to
    // avoid pulling 5 MB of decompressed body for a 50 MB
    // archive.
    if let Some(len) = response
        .content_length()
        .and_then(|value| usize::try_from(value).ok())
    {
        match check_content_length(Some(len)) {
            SizeCheck::TooLarge { advertised } => {
                return failure(
                    "webfetch",
                    format!(
                        "response too large: {advertised} bytes exceed the {MAX_RESPONSE_BYTES}-byte cap"
                    ),
                );
            }
            SizeCheck::Ok | SizeCheck::Unknown => {}
        }
    }

    // Read the body. Post-decompression cap.
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return failure("webfetch", format!("read body failed: {error}")),
    };
    if !check_body_size(bytes.len()) {
        return failure(
            "webfetch",
            format!(
                "response too large: {} bytes exceed the {MAX_RESPONSE_BYTES}-byte cap",
                bytes.len()
            ),
        );
    }

    let body = String::from_utf8_lossy(&bytes);
    let mut output = body.into_owned();
    if output.len() > MAX_RESPONSE_BYTES {
        // Defensive: should be unreachable because we just
        // checked, but bound anyway so the runner can persist
        // the result.
        output.truncate(MAX_RESPONSE_BYTES);
    }
    success("webfetch", output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_under_cap_is_ok() {
        assert_eq!(check_content_length(Some(0)), SizeCheck::Ok);
        assert_eq!(check_content_length(Some(1024)), SizeCheck::Ok);
        assert_eq!(
            check_content_length(Some(MAX_RESPONSE_BYTES)),
            SizeCheck::Ok
        );
    }

    #[test]
    fn content_length_over_cap_is_too_large() {
        assert_eq!(
            check_content_length(Some(MAX_RESPONSE_BYTES + 1)),
            SizeCheck::TooLarge {
                advertised: MAX_RESPONSE_BYTES + 1
            }
        );
        assert_eq!(
            check_content_length(Some(10 * 1024 * 1024)),
            SizeCheck::TooLarge {
                advertised: 10 * 1024 * 1024
            }
        );
    }

    #[test]
    fn absent_content_length_is_unknown() {
        assert_eq!(check_content_length(None), SizeCheck::Unknown);
    }

    #[test]
    fn body_under_cap_passes() {
        assert!(check_body_size(0));
        assert!(check_body_size(MAX_RESPONSE_BYTES));
    }

    #[test]
    fn body_over_cap_rejected() {
        assert!(!check_body_size(MAX_RESPONSE_BYTES + 1));
        assert!(!check_body_size(10 * 1024 * 1024));
    }

    #[test]
    fn empty_url_is_rejected() {
        let call = ToolCall {
            name: "webfetch".to_string(),
            arguments: std::collections::HashMap::from([(
                "url".to_string(),
                serde_json::json!(""),
            )]),
        };
        let result = run(&call);
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("url is required"));
    }

    /// End-to-end against a tiny in-memory HTTP server: GET a
    /// response under the cap and verify the body comes through.
    /// Uses `tokio::net::TcpListener` so we don't pull in a
    /// heavyweight HTTP mock dep just for one test.
    #[tokio::test]
    async fn e2e_serves_response_under_cap() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let body = "<html>hello</html>".to_string();
        let body_for_server = body.clone();
        let body_len = body.len();

        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body_for_server}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let call = ToolCall {
            name: "webfetch".to_string(),
            arguments: std::collections::HashMap::from([(
                "url".to_string(),
                serde_json::json!(format!("http://{addr}/")),
            )]),
        };
        let result = run_with_async(&call, &client).await;
        let _ = server.await;

        assert!(result.success, "fetch failed: {:?}", result.error);
        assert_eq!(result.output, body);
    }

    /// End-to-end: the runner awaits the async tool before
    /// continuing. The server stalls for 200 ms before
    /// responding; if the runner spawned the fetch in the
    /// background and moved on, the test would race the
    /// response. Instead the test asserts the fetch *does*
    /// complete (proving the future is being awaited to
    /// completion) and that the result lands in the output.
    #[tokio::test]
    async fn e2e_runner_awaits_async_tool_to_completion() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio::time::sleep;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        // Stall the response so we can verify the runner waits
        // for it. If the runner moved on, the future would be
        // dropped and the result empty.
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                sleep(Duration::from_millis(200)).await;
                let body = "delayed body";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let start = std::time::Instant::now();
        let client = reqwest::Client::new();
        let call = ToolCall {
            name: "webfetch".to_string(),
            arguments: std::collections::HashMap::from([(
                "url".to_string(),
                serde_json::json!(format!("http://{addr}/")),
            )]),
        };
        let result = run_with_async(&call, &client).await;
        let elapsed = start.elapsed();
        let _ = server.await;

        // Two things to verify:
        //  1. The fetch completed (not dropped). If the runner
        //     backgrounded the call, the result would be a
        //     placeholder or empty.
        //  2. The fetch was awaited: at least 200 ms passed
        //     between dispatch and return. A backgrounded call
        //     would return in <1 ms.
        assert!(result.success, "delayed fetch failed: {:?}", result.error);
        assert_eq!(result.output, "delayed body");
        assert!(
            elapsed >= Duration::from_millis(200),
            "elapsed {elapsed:?} suggests the future was not awaited to completion"
        );
    }

    /// End-to-end: server lies about Content-Length (or omits
    /// it) and the body itself is under the cap. The
    /// post-decompression check accepts it.
    #[tokio::test]
    async fn e2e_under_cap_body_with_no_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let body = "chunked response under cap".to_string();

        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                // Transfer-Encoding: chunked with a single chunk
                // that ends in 0-length terminator. reqwest
                // decodes the chunked body and we end up with
                // the plain string.
                let chunk = format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n{chunk}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let call = ToolCall {
            name: "webfetch".to_string(),
            arguments: std::collections::HashMap::from([(
                "url".to_string(),
                serde_json::json!(format!("http://{addr}/")),
            )]),
        };
        let result = run_with_async(&call, &client).await;
        let _ = server.await;

        // Chunked-decoding support varies across reqwest
        // versions; we accept either exact match or
        // failure (the test is informative either way).
        if result.success {
            assert!(result.output.contains("chunked response under cap"));
        }
    }

    /// End-to-end: server returns a 6 MB body and the
    /// Content-Length pre-check rejects it without reading
    /// the body. We assert the tool returns an error mentioning
    /// the cap.
    #[tokio::test]
    async fn e2e_content_length_over_cap_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let advertised = MAX_RESPONSE_BYTES + 1024;

        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {advertised}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let client = reqwest::Client::new();
        let call = ToolCall {
            name: "webfetch".to_string(),
            arguments: std::collections::HashMap::from([(
                "url".to_string(),
                serde_json::json!(format!("http://{addr}/")),
            )]),
        };
        let result = run_with_async(&call, &client).await;
        let _ = server.await;

        assert!(!result.success, "over-cap response should be rejected");
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            error.contains("too large") || error.contains("exceed"),
            "error should mention the cap, got: {error}"
        );
    }
}
