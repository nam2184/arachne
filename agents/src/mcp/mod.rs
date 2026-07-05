use std::collections::{hash_map::DefaultHasher, BTreeMap};
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::config::{McpServerConfig, McpTransport, RuntimeConfig};
use crate::llm::events::ToolDefinition;
use crate::{ToolCall, ToolResult};

const TOOL_PREFIX: &str = "mcp__";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Clone, Deserialize)]
struct McpListToolsResult {
    #[serde(default)]
    tools: Vec<McpTool>,
}

#[derive(Debug, Clone, Deserialize)]
struct McpTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct McpCallResult {
    #[serde(default)]
    content: Vec<McpContent>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct McpContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, Value>,
}

struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: u64,
}

type StdioClientHandle = Arc<Mutex<StdioClient>>;

struct SseEvent {
    event: Option<String>,
    data: String,
}

struct SseStream {
    chunks: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: String,
}

#[derive(Clone, Default)]
pub struct McpManager {
    stdio_clients: Arc<Mutex<BTreeMap<String, StdioClientHandle>>>,
    tools: Arc<Mutex<BTreeMap<String, Vec<McpTool>>>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn tool_definitions(
        &self,
        config: &RuntimeConfig,
        readonly: bool,
    ) -> Vec<ToolDefinition> {
        if readonly {
            return Vec::new();
        }

        let mut definitions = Vec::new();
        for (server_name, server) in enabled_servers(config) {
            let discovered = match timeout(DISCOVERY_TIMEOUT, self.list_tools(server_name, server))
                .await
            {
                Ok(Ok(tools)) => tools,
                Ok(Err(error)) => {
                    tracing::warn!(server = %server_name, error = %error, "mcp tool discovery failed");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(server = %server_name, "mcp tool discovery timed out");
                    continue;
                }
            };

            for tool in discovered {
                let name = tool_name_for(server_name, &tool.name);
                definitions.push(ToolDefinition::new(
                    &name,
                    &tool.description.unwrap_or_else(|| {
                        format!("Run MCP tool `{}` from server `{server_name}`", tool.name)
                    }),
                    normalize_schema(tool.input_schema),
                ));
            }
        }

        definitions.sort_by(|a, b| a.name.cmp(&b.name));
        definitions
    }

    pub async fn run_tool_call(&self, call: &ToolCall, config: &RuntimeConfig) -> ToolResult {
        let Some((server_part, tool_part)) = parse_tool_name(&call.name) else {
            return failure(&call.name, "invalid MCP tool name".to_string());
        };

        let Some((server_name, server)) = enabled_servers(config)
            .into_iter()
            .find(|(name, _)| sanitize_name(name) == server_part)
        else {
            return failure(
                &call.name,
                format!("MCP server not configured: {server_part}"),
            );
        };

        let discovered =
            match timeout(DISCOVERY_TIMEOUT, self.list_tools(server_name, server)).await {
                Ok(Ok(tools)) => tools,
                Ok(Err(error)) => return failure(&call.name, error),
                Err(_) => return failure(&call.name, "MCP tool discovery timed out".to_string()),
            };

        let Some(tool) = discovered
            .iter()
            .find(|tool| sanitize_name(&tool.name) == tool_part)
        else {
            return failure(
                &call.name,
                format!("MCP tool not found on {server_name}: {tool_part}"),
            );
        };

        let arguments = Value::Object(call.arguments.clone().into_iter().collect());
        match timeout(
            CALL_TIMEOUT,
            self.call_tool(server_name, server, &tool.name, arguments),
        )
        .await
        {
            Ok(Ok(result)) => tool_result(&call.name, server_name, &tool.name, result),
            Ok(Err(error)) => failure(&call.name, error),
            Err(_) => failure(&call.name, "MCP tool call timed out".to_string()),
        }
    }

    async fn list_tools(
        &self,
        server_name: &str,
        server: &McpServerConfig,
    ) -> Result<Vec<McpTool>, String> {
        let cache_key = server_cache_key(server_name, server);
        if let Some(tools) = self.tools.lock().await.get(&cache_key).cloned() {
            return Ok(tools);
        }

        let result = self
            .request(&cache_key, server, "tools/list", serde_json::json!({}))
            .await?;

        let tools = serde_json::from_value::<McpListToolsResult>(result)
            .map(|result| result.tools)
            .map_err(|error| format!("parse MCP tools/list result: {error}"))?;
        self.tools.lock().await.insert(cache_key, tools.clone());
        Ok(tools)
    }

    async fn call_tool(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpCallResult, String> {
        let cache_key = server_cache_key(server_name, server);
        let result = self
            .request(
                &cache_key,
                server,
                "tools/call",
                serde_json::json!({
                    "name": tool_name,
                    "arguments": arguments,
                }),
            )
            .await?;

        serde_json::from_value::<McpCallResult>(result)
            .map_err(|error| format!("parse MCP tools/call result: {error}"))
    }

    async fn request(
        &self,
        cache_key: &str,
        server: &McpServerConfig,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        match server.transport {
            McpTransport::Stdio => self.stdio_request(cache_key, server, method, params).await,
            McpTransport::StreamableHttp => http_rpc_request(server, method, params, true).await,
            McpTransport::PollingHttp => http_rpc_request(server, method, params, false).await,
            McpTransport::Sse => legacy_sse_request(server, method, params).await,
        }
    }

    async fn stdio_request(
        &self,
        cache_key: &str,
        server: &McpServerConfig,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let client = self.stdio_client_for(cache_key, server).await?;
        let result = {
            let mut client = client.lock().await;
            client.request(method, params).await
        };

        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                self.remove_stdio_client(cache_key, &client).await;
                Err(error)
            }
        }
    }

    async fn stdio_client_for(
        &self,
        cache_key: &str,
        server: &McpServerConfig,
    ) -> Result<StdioClientHandle, String> {
        if let Some(client) = self.stdio_clients.lock().await.get(cache_key).cloned() {
            return Ok(client);
        }

        let mut client = StdioClient::start(server).await?;
        client.initialize().await?;
        let client = Arc::new(Mutex::new(client));

        let mut clients = self.stdio_clients.lock().await;
        if let Some(existing) = clients.get(cache_key).cloned() {
            return Ok(existing);
        }

        clients.insert(cache_key.to_string(), Arc::clone(&client));
        Ok(client)
    }

    async fn remove_stdio_client(&self, cache_key: &str, client: &StdioClientHandle) {
        let mut clients = self.stdio_clients.lock().await;
        if clients
            .get(cache_key)
            .is_some_and(|current| Arc::ptr_eq(current, client))
        {
            clients.remove(cache_key);
        }
        self.tools.lock().await.remove(cache_key);
    }
}

pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with(TOOL_PREFIX)
}

pub async fn tool_definitions(config: &RuntimeConfig, readonly: bool) -> Vec<ToolDefinition> {
    McpManager::new().tool_definitions(config, readonly).await
}

pub async fn run_tool_call(call: &ToolCall, config: &RuntimeConfig) -> ToolResult {
    McpManager::new().run_tool_call(call, config).await
}

fn enabled_servers(config: &RuntimeConfig) -> Vec<(&String, &McpServerConfig)> {
    config
        .mcp
        .servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .collect()
}

impl StdioClient {
    async fn start(server: &McpServerConfig) -> Result<Self, String> {
        let command = server
            .command
            .as_deref()
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| "MCP stdio server requires command".to_string())?;

        let mut child = Command::new(command);
        child.args(&server.args);
        child.envs(&server.env);
        if let Some(cwd) = server.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
            child.current_dir(cwd);
        }
        child.stdin(Stdio::piped());
        child.stdout(Stdio::piped());
        child.stderr(Stdio::null());

        let mut child = child
            .spawn()
            .map_err(|error| format!("start MCP stdio server {command}: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP stdio server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP stdio server stdout unavailable".to_string())?;

        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "arachne",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await?;
        self.notify("notifications/initialized", serde_json::json!({}))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(
            &mut self.stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }),
        )
        .await?;

        loop {
            let message = read_message(&mut self.stdout).await?;
            let response: RpcResponse = serde_json::from_value(message)
                .map_err(|error| format!("parse MCP response: {error}"))?;
            if response.id.as_ref().and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.error {
                return Err(error.message);
            }
            return response
                .result
                .ok_or_else(|| format!("MCP response for {method} missing result"));
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        write_message(
            &mut self.stdin,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
        )
        .await
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn http_rpc_request(
    server: &McpServerConfig,
    method: &str,
    params: Value,
    streamable: bool,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let url = mcp_url(server)?;
    let headers = header_map(server)?;
    let mut session_id = None;

    let initialize = rpc_request(1, "initialize", initialize_params());
    let initialize_response = post_json_rpc(
        &client,
        &url,
        &headers,
        session_id.as_deref(),
        initialize,
        streamable,
    )
    .await?;
    session_id = initialize_response.session_id;
    if let Some(value) = initialize_response.body {
        rpc_result(value, 1, "initialize")?;
    }

    let _ = post_json_rpc(
        &client,
        &url,
        &headers,
        session_id.as_deref(),
        rpc_notification("notifications/initialized", serde_json::json!({})),
        streamable,
    )
    .await?;

    let response = post_json_rpc(
        &client,
        &url,
        &headers,
        session_id.as_deref(),
        rpc_request(2, method, params),
        streamable,
    )
    .await?;
    rpc_result(
        response
            .body
            .ok_or_else(|| format!("MCP response for {method} missing body"))?,
        2,
        method,
    )
}

async fn legacy_sse_request(
    server: &McpServerConfig,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let url = mcp_url(server)?;
    let headers = header_map(server)?;
    let response = client
        .get(&url)
        .headers(headers.clone())
        .header(ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|error| format!("connect MCP SSE endpoint: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "connect MCP SSE endpoint returned HTTP {}",
            response.status()
        ));
    }

    let mut stream = SseStream::new(response);
    let endpoint = read_sse_endpoint(&mut stream).await?;
    let post_url = sse_post_url(&url, &endpoint)?;

    post_sse_json(
        &client,
        &post_url,
        &headers,
        rpc_request(1, "initialize", initialize_params()),
    )
    .await?;
    let initialize_result = read_rpc_result_from_sse(&mut stream, 1, "initialize").await?;
    drop(initialize_result);

    post_sse_json(
        &client,
        &post_url,
        &headers,
        rpc_notification("notifications/initialized", serde_json::json!({})),
    )
    .await?;

    post_sse_json(&client, &post_url, &headers, rpc_request(2, method, params)).await?;
    read_rpc_result_from_sse(&mut stream, 2, method).await
}

struct HttpRpcResponse {
    body: Option<Value>,
    session_id: Option<String>,
}

async fn post_json_rpc(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    session_id: Option<&str>,
    body: Value,
    streamable: bool,
) -> Result<HttpRpcResponse, String> {
    let mut request = client
        .post(url)
        .headers(headers.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(
            ACCEPT,
            if streamable {
                "application/json, text/event-stream"
            } else {
                "application/json"
            },
        )
        .json(&body);
    if let Some(session_id) = session_id {
        request = request.header("mcp-session-id", session_id);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("send MCP HTTP request: {error}"))?;
    let status = response.status();
    let returned_session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if status.as_u16() == 202 || status.as_u16() == 204 {
        return Ok(HttpRpcResponse {
            body: None,
            session_id: returned_session_id,
        });
    }
    if !status.is_success() {
        return Err(format!("MCP HTTP request returned HTTP {status}"));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("read MCP HTTP response: {error}"))?;
    if bytes.is_empty() {
        return Ok(HttpRpcResponse {
            body: None,
            session_id: returned_session_id,
        });
    }

    let body = if content_type.contains("text/event-stream") || looks_like_sse(&bytes) {
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|error| format!("parse MCP SSE HTTP response: {error}"))?;
        sse_events_from_text(&text)
            .into_iter()
            .find_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .ok_or_else(|| "MCP SSE HTTP response missing JSON data".to_string())?
    } else {
        serde_json::from_slice(&bytes).map_err(|error| format!("parse MCP HTTP body: {error}"))?
    };

    Ok(HttpRpcResponse {
        body: Some(body),
        session_id: returned_session_id,
    })
}

async fn post_sse_json(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    body: Value,
) -> Result<(), String> {
    let response = client
        .post(url)
        .headers(headers.clone())
        .header(CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("send MCP SSE message: {error}"))?;
    if response.status().is_success() || response.status().as_u16() == 202 {
        Ok(())
    } else {
        Err(format!(
            "MCP SSE message returned HTTP {}",
            response.status()
        ))
    }
}

async fn read_sse_endpoint(stream: &mut SseStream) -> Result<String, String> {
    loop {
        let event = stream.next_event().await?;
        if event.event.as_deref() == Some("endpoint") {
            return Ok(event.data.trim().to_string());
        }
    }
}

async fn read_rpc_result_from_sse(
    stream: &mut SseStream,
    id: u64,
    method: &str,
) -> Result<Value, String> {
    loop {
        let event = stream.next_event().await?;
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        if rpc_response_id(&value) == Some(id) {
            return rpc_result(value, id, method);
        }
    }
}

impl SseStream {
    fn new(response: reqwest::Response) -> Self {
        Self {
            chunks: Box::pin(response.bytes_stream()),
            buffer: String::new(),
        }
    }

    async fn next_event(&mut self) -> Result<SseEvent, String> {
        loop {
            if let Some((event, drain_len)) = parse_next_sse_event(&self.buffer) {
                self.buffer.drain(..drain_len);
                return Ok(event);
            }

            let chunk = self
                .chunks
                .next()
                .await
                .ok_or_else(|| "MCP SSE stream closed".to_string())?
                .map_err(|error| format!("read MCP SSE stream: {error}"))?;
            let text = std::str::from_utf8(&chunk)
                .map_err(|error| format!("parse MCP SSE stream: {error}"))?;
            self.buffer.push_str(text);
        }
    }
}

fn parse_next_sse_event(buffer: &str) -> Option<(SseEvent, usize)> {
    let (index, separator_len) = match (buffer.find("\r\n\r\n"), buffer.find("\n\n")) {
        (Some(crlf), Some(lf)) if crlf < lf => (crlf, 4),
        (Some(_), Some(lf)) => (lf, 2),
        (Some(crlf), None) => (crlf, 4),
        (None, Some(lf)) => (lf, 2),
        (None, None) => return None,
    };
    let raw = &buffer[..index];
    Some((parse_sse_event(raw), index + separator_len))
}

fn parse_sse_event(raw: &str) -> SseEvent {
    let mut event = None;
    let mut data = Vec::new();
    for line in raw.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_string());
        }
    }
    SseEvent {
        event,
        data: data.join("\n"),
    }
}

fn sse_events_from_text(text: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut rest = text.to_string();
    while let Some((event, drain_len)) = parse_next_sse_event(&rest) {
        rest.drain(..drain_len);
        events.push(event);
    }
    events
}

fn looks_like_sse(bytes: &[u8]) -> bool {
    bytes.starts_with(b"event:") || bytes.starts_with(b"data:") || bytes.starts_with(b":")
}

fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn rpc_notification(method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "arachne",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn rpc_result(value: Value, id: u64, method: &str) -> Result<Value, String> {
    let response: RpcResponse =
        serde_json::from_value(value).map_err(|error| format!("parse MCP response: {error}"))?;
    if response.id.as_ref().and_then(Value::as_u64) != Some(id) {
        return Err(format!("MCP response for {method} has unexpected id"));
    }
    if let Some(error) = response.error {
        return Err(error.message);
    }
    response
        .result
        .ok_or_else(|| format!("MCP response for {method} missing result"))
}

fn rpc_response_id(value: &Value) -> Option<u64> {
    value.get("id").and_then(Value::as_u64)
}

fn mcp_url(server: &McpServerConfig) -> Result<String, String> {
    let url = server
        .url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| format!("MCP {:?} server requires url", server.transport))?;
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("parse MCP URL: {error}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed.to_string()),
        _ => Err("MCP URL must use http or https".to_string()),
    }
}

fn sse_post_url(base: &str, endpoint: &str) -> Result<String, String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err("MCP SSE endpoint event was empty".to_string());
    }
    if let Ok(url) = reqwest::Url::parse(endpoint) {
        return Ok(url.to_string());
    }
    let base = reqwest::Url::parse(base).map_err(|error| format!("parse MCP SSE URL: {error}"))?;
    base.join(endpoint)
        .map(|url| url.to_string())
        .map_err(|error| format!("resolve MCP SSE endpoint: {error}"))
}

fn header_map(server: &McpServerConfig) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (name, value) in &server.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid MCP header name `{name}`: {error}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| format!("invalid MCP header value for `{name}`: {error}"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

async fn write_message(writer: &mut ChildStdin, value: &Value) -> Result<(), String> {
    let body =
        serde_json::to_vec(value).map_err(|error| format!("serialize MCP request: {error}"))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|error| format!("write MCP header: {error}"))?;
    writer
        .write_all(&body)
        .await
        .map_err(|error| format!("write MCP body: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("flush MCP request: {error}"))
}

async fn read_message(reader: &mut (impl AsyncRead + Unpin)) -> Result<Value, String> {
    let mut header = Vec::new();
    let mut byte = [0; 1];
    loop {
        let read = reader
            .read(&mut byte)
            .await
            .map_err(|error| format!("read MCP header: {error}"))?;
        if read == 0 {
            return Err("MCP server closed stdout".to_string());
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let header = String::from_utf8(header).map_err(|error| format!("parse MCP header: {error}"))?;
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| "MCP response missing Content-Length".to_string())?;

    let mut body = vec![0; content_length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| format!("read MCP body: {error}"))?;
    serde_json::from_slice(&body).map_err(|error| format!("parse MCP body: {error}"))
}

fn tool_result(
    advertised_name: &str,
    server_name: &str,
    tool_name: &str,
    result: McpCallResult,
) -> ToolResult {
    let output = result
        .content
        .into_iter()
        .map(|content| match (content.kind.as_str(), content.text) {
            ("text", Some(text)) => text,
            _ => serde_json::to_string(&content.rest).unwrap_or_else(|_| "{}".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error {
        return failure(advertised_name, output);
    }

    ToolResult {
        tool: advertised_name.to_string(),
        success: true,
        output,
        error: None,
        metadata: Some(serde_json::json!({
            "kind": "mcp",
            "server": server_name,
            "tool": tool_name,
        })),
    }
}

fn failure(tool: &str, error: String) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: false,
        output: String::new(),
        error: Some(error),
        metadata: None,
    }
}

fn normalize_schema(schema: Value) -> Value {
    match schema {
        Value::Object(_) => schema,
        _ => serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true,
        }),
    }
}

fn tool_name_for(server_name: &str, tool_name: &str) -> String {
    format!(
        "{TOOL_PREFIX}{}__{}",
        sanitize_name(server_name),
        sanitize_name(tool_name)
    )
}

fn parse_tool_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix(TOOL_PREFIX)?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| (server.to_string(), tool.to_string()))
}

fn sanitize_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "tool".to_string()
    } else {
        sanitized
    }
}

fn server_cache_key(server_name: &str, server: &McpServerConfig) -> String {
    let mut hasher = DefaultHasher::new();
    server_name.hash(&mut hasher);
    if let Ok(serialized) = serde_json::to_string(server) {
        serialized.hash(&mut hasher);
    }
    format!("{}:{:016x}", sanitize_name(server_name), hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::config::RuntimeConfigFile;

    #[test]
    fn mcp_tool_names_are_namespaced_and_sanitized() {
        assert_eq!(
            tool_name_for("local fs", "read.file"),
            "mcp__local_fs__read_file"
        );
        assert!(is_mcp_tool_name("mcp__local_fs__read_file"));
        assert_eq!(
            parse_tool_name("mcp__local_fs__read_file"),
            Some(("local_fs".to_string(), "read_file".to_string()))
        );
    }

    #[test]
    fn non_object_schema_gets_empty_object_schema() {
        assert_eq!(
            normalize_schema(Value::Null),
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": true,
            })
        );
    }

    #[tokio::test]
    async fn manager_reuses_stdio_server_between_calls() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mcp_server.py");
        std::fs::write(
            &script,
            r#"
import json
import sys

call_count = 0

def read_message():
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        byte = sys.stdin.buffer.read(1)
        if not byte:
            sys.exit(0)
        header += byte
    length = None
    for line in header.decode().splitlines():
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
            break
    if length is None:
        sys.exit(1)
    return json.loads(sys.stdin.buffer.read(length))

def write_message(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    method = message.get("method")
    if "id" not in message:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fixture", "version": "1"}}
        write_message({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {}}}]}
        write_message({"jsonrpc": "2.0", "id": message["id"], "result": result})
    elif method == "tools/call":
        call_count += 1
        result = {"content": [{"type": "text", "text": f"call_count={call_count}"}], "isError": False}
        write_message({"jsonrpc": "2.0", "id": message["id"], "result": result})
    else:
        error = {"code": -32601, "message": f"unknown method: {method}"}
        write_message({"jsonrpc": "2.0", "id": message["id"], "error": error})
"#,
        )
        .unwrap();

        let mut config = RuntimeConfigFile::default();
        config.mcp.servers.insert(
            "fixture".to_string(),
            McpServerConfig {
                command: Some("python3".to_string()),
                args: vec![script.display().to_string()],
                ..McpServerConfig::default()
            },
        );

        let manager = McpManager::new();
        let call = ToolCall {
            name: tool_name_for("fixture", "echo"),
            arguments: HashMap::new(),
        };

        let first = manager.run_tool_call(&call, &config).await;
        let second = manager.run_tool_call(&call, &config).await;

        assert!(first.success, "first result: {first:?}");
        assert!(second.success, "second result: {second:?}");
        assert_eq!(first.output, "call_count=1");
        assert_eq!(second.output, "call_count=2");
    }
}
