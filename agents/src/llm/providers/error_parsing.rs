//! Shared helpers for parsing OpenAI-compatible provider error
//! responses into a structured shape so we can surface
//! `[type] message (code, param)` instead of a wall of JSON to
//! the user. Both the HTTP backend and the AI SDK backend
//! surface errors through this module so the user-visible
//! message stays consistent.
//!
//! Recognized shapes:
//! - OpenAI: `{"error":{"message":"…","type":"…","code":"…","param":"…","request_id":"…","details":"…"}}`
//! - OpenAI string: `{"error":"…"}`
//! - Anthropic: `{"type":"invalid_request_error","message":"…","error_code":"…"}`
//! - Raw text body (non-JSON) — surfaced verbatim.
//! - Empty body — surfaced as `<empty response body>`.
//!
//! Anything else falls back to `kind = "unknown"` and the raw
//! trimmed body so the operator sees exactly what the
//! provider returned.

#[derive(Debug)]
pub(crate) struct ProviderErrorInfo {
    pub kind: String,
    pub message: String,
    pub error_type: Option<String>,
    pub error_code: Option<String>,
    pub error_param: Option<String>,
}

pub(crate) fn parse_provider_error_body(body: &str) -> ProviderErrorInfo {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return ProviderErrorInfo {
            kind: "empty".to_string(),
            message: "<empty response body>".to_string(),
            error_type: None,
            error_code: None,
            error_param: None,
        };
    }

    let parsed: serde_json::Value = match serde_json::from_str(trimmed).or_else(|_| {
        embedded_json_object(trimmed)
            .ok_or(())
            .and_then(|json| serde_json::from_str(json).map_err(|_| ()))
    }) {
        Ok(value) => value,
        Err(_) => {
            return ProviderErrorInfo {
                kind: "raw".to_string(),
                message: trimmed.to_string(),
                error_type: None,
                error_code: None,
                error_param: None,
            };
        }
    };

    // OpenAI shape: `{"error": {"message": …, "type": …, "code": …}}`.
    let obj = parsed.as_object();
    if let Some(error) = obj.and_then(|map| map.get("error")) {
        if let Some(error_obj) = error.as_object() {
            return ProviderErrorInfo {
                kind: "structured".to_string(),
                message: error_obj
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                error_type: error_obj
                    .get("type")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                error_code: error_obj
                    .get("code")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                error_param: error_obj
                    .get("param")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            };
        }
        if let Some(text) = error.as_str() {
            return ProviderErrorInfo {
                kind: "string".to_string(),
                message: text.to_string(),
                error_type: None,
                error_code: None,
                error_param: None,
            };
        }
    }

    // Anthropic Messages shape: top-level `type` + `message`.
    if let Some(map) = obj {
        let message = map
            .get("message")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !message.is_empty() {
            return ProviderErrorInfo {
                kind: "anthropic".to_string(),
                message: message.to_string(),
                error_type: map
                    .get("type")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                error_code: map
                    .get("error_code")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                error_param: None,
            };
        }
    }

    ProviderErrorInfo {
        kind: "unknown".to_string(),
        message: trimmed.to_string(),
        error_type: None,
        error_code: None,
        error_param: None,
    }
}

fn embedded_json_object(value: &str) -> Option<&str> {
    let start = value.find('{')?;
    let end = value.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&value[start..=end])
}

/// Render the provider error into a one-line human-readable
/// string. We prefer `[type] message (code, param)` when we have
/// a structured shape and fall back to the raw body otherwise.
pub(crate) fn format_provider_error(info: &ProviderErrorInfo, raw_body: &str) -> String {
    match info.kind.as_str() {
        "structured" | "anthropic" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(error_type) = &info.error_type {
                if !error_type.is_empty() {
                    parts.push(format!("[{error_type}]"));
                }
            }
            if let Some(code) = &info.error_code {
                if !code.is_empty() {
                    parts.push(format!("code={code}"));
                }
            }
            if let Some(param) = &info.error_param {
                if !param.is_empty() {
                    parts.push(format!("param={param}"));
                }
            }
            parts.push(info.message.clone());
            let prefix = parts.join(" ");
            if prefix.is_empty() {
                raw_body.to_string()
            } else {
                prefix
            }
        }
        _ => info.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_structured_error() {
        let body = r#"{"error":{"message":"Invalid API key","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        let info = parse_provider_error_body(body);
        assert_eq!(info.kind, "structured");
        assert_eq!(info.message, "Invalid API key");
        assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(info.error_code.as_deref(), Some("invalid_api_key"));
        assert_eq!(
            format_provider_error(&info, body),
            "[invalid_request_error] code=invalid_api_key Invalid API key"
        );
    }

    #[test]
    fn parses_openai_string_error() {
        let body = r#"{"error":"plain text error"}"#;
        let info = parse_provider_error_body(body);
        assert_eq!(info.kind, "string");
        assert_eq!(info.message, "plain text error");
    }

    #[test]
    fn parses_anthropic_error() {
        let body = r#"{"type":"invalid_request_error","message":"messages: field required"}"#;
        let info = parse_provider_error_body(body);
        assert_eq!(info.kind, "anthropic");
        assert_eq!(info.message, "messages: field required");
        assert_eq!(info.error_type.as_deref(), Some("invalid_request_error"));
        assert_eq!(
            format_provider_error(&info, body),
            "[invalid_request_error] messages: field required"
        );
    }

    #[test]
    fn falls_back_to_raw_body_when_not_json() {
        let body = "Bad Request\nSome HTML error page";
        let info = parse_provider_error_body(body);
        assert_eq!(info.kind, "raw");
        assert_eq!(info.message, body);
    }

    #[test]
    fn empty_body_surfaces_as_marker() {
        let info = parse_provider_error_body("");
        assert_eq!(info.kind, "empty");
        assert_eq!(info.message, "<empty response body>");
    }
}
