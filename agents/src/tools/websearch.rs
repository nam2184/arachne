use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::{ToolCall, ToolResult};

use super::{failure, string_arg, success, usize_arg};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESULTS: usize = 5;
const HARD_MAX_RESULTS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchConfig {
    pub base_url: String,
    pub max_results: usize,
}

#[derive(Debug, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Debug, Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default, alias = "content")]
    snippet: String,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    published_date: Option<String>,
}

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    query: String,
    results: Vec<WebSearchResult>,
}

#[derive(Debug, Serialize)]
struct WebSearchResult {
    title: String,
    url: String,
    snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_date: Option<String>,
}

pub fn run(call: &ToolCall) -> ToolResult {
    let query = string_arg(call, "query");
    if query.trim().is_empty() {
        return failure("websearch", "query is required".to_string());
    }
    failure(
        "websearch",
        "websearch requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string(),
    )
}

pub async fn run_async(call: &ToolCall) -> ToolResult {
    let config = match config_from_env() {
        Ok(config) => config,
        Err(error) => return failure("websearch", error),
    };
    run_with_async(call, &reqwest::Client::new(), &config).await
}

pub async fn run_async_with_runtime_config(
    call: &ToolCall,
    runtime_config: &crate::config::RuntimeConfig,
) -> ToolResult {
    let config = match config_from_runtime(runtime_config).or_else(|_| config_from_env()) {
        Ok(config) => config,
        Err(error) => return failure("websearch", error),
    };
    run_with_async(call, &reqwest::Client::new(), &config).await
}

pub async fn run_with_async(
    call: &ToolCall,
    client: &reqwest::Client,
    config: &WebSearchConfig,
) -> ToolResult {
    let query = string_arg(call, "query");
    let query = query.trim();
    if query.is_empty() {
        return failure("websearch", "query is required".to_string());
    }

    let requested_limit = usize_arg(call, "limit").unwrap_or(config.max_results);
    let limit = requested_limit
        .min(config.max_results)
        .clamp(1, HARD_MAX_RESULTS);

    let url = match build_search_url(&config.base_url, query) {
        Ok(url) => url,
        Err(error) => return failure("websearch", error),
    };

    let response = match client
        .get(url.clone())
        .timeout(DEFAULT_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return failure("websearch", format!("request failed: {error}")),
    };

    let status = response.status();
    if !status.is_success() {
        return failure("websearch", format!("HTTP {} for {}", status.as_u16(), url));
    }

    let body = match response.json::<SearxngResponse>().await {
        Ok(body) => body,
        Err(error) => return failure("websearch", format!("parse response failed: {error}")),
    };

    let results = body
        .results
        .into_iter()
        .filter(|result| !result.url.trim().is_empty())
        .take(limit)
        .map(|result| WebSearchResult {
            title: result.title,
            url: result.url,
            snippet: result.snippet,
            engine: result.engine,
            score: result.score,
            category: result.category,
            published_date: result.published_date,
        })
        .collect();

    let output = WebSearchOutput {
        query: query.to_string(),
        results,
    };
    match serde_json::to_string_pretty(&output) {
        Ok(output) => success("websearch", output),
        Err(error) => failure("websearch", format!("serialize output failed: {error}")),
    }
}

pub fn config_from_env() -> Result<WebSearchConfig, String> {
    let base_url = std::env::var("SEARXNG_BASE_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "websearch requires SEARXNG_BASE_URL or a SearXNG base URL in app settings".to_string()
        })?;

    let max_results = std::env::var("SEARXNG_MAX_RESULTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, HARD_MAX_RESULTS);

    Ok(WebSearchConfig {
        base_url,
        max_results,
    })
}

pub fn config_from_runtime(
    config: &crate::config::RuntimeConfig,
) -> Result<WebSearchConfig, String> {
    let base_url = config
        .websearch
        .searxng_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "websearch requires a SearXNG base URL in runtime config or SEARXNG_BASE_URL"
                .to_string()
        })?
        .to_string();

    let max_results = config
        .websearch
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, HARD_MAX_RESULTS);

    Ok(WebSearchConfig {
        base_url,
        max_results,
    })
}

pub fn build_search_url(base_url: &str, query: &str) -> Result<Url, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("SearXNG base URL is required".to_string());
    }

    let mut url = Url::parse(&format!("{trimmed}/search"))
        .map_err(|error| format!("invalid SearXNG base URL: {error}"))?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json");
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    fn call(args: &[(&str, serde_json::Value)]) -> ToolCall {
        ToolCall {
            name: "websearch".to_string(),
            arguments: args
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn empty_query_is_rejected() {
        let result = run(&call(&[("query", json!(""))]));
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("query is required"));
    }

    #[test]
    fn search_url_targets_json_api() {
        let url = build_search_url("https://search.example.com/", "rust tauri").unwrap();
        assert_eq!(
            url.as_str(),
            "https://search.example.com/search?q=rust+tauri&format=json"
        );
    }

    #[tokio::test]
    async fn parses_searxng_response_and_caps_results() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let body = json!({
            "results": [
                { "title": "One", "url": "https://example.com/1", "content": "first", "engine": "test" },
                { "title": "Two", "url": "https://example.com/2", "content": "second" }
            ]
        })
        .to_string();
        let body_len = body.len();

        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let config = WebSearchConfig {
            base_url: format!("http://{addr}"),
            max_results: 1,
        };
        let result = run_with_async(
            &call(&[("query", json!("rust")), ("limit", json!(10))]),
            &reqwest::Client::new(),
            &config,
        )
        .await;
        let _ = server.await;

        assert!(result.success, "result: {result:?}");
        assert!(result.output.contains("One"));
        assert!(!result.output.contains("Two"));
    }
}
