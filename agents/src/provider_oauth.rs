use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use uuid::Uuid;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOAuthAuthorization {
    pub provider_name: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct ProviderOAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

#[derive(Clone)]
struct ProviderOAuthConfig {
    provider_name: &'static str,
    client_id: &'static str,
    issuer: &'static str,
    scopes: &'static str,
    callback_port: u16,
    callback_path: &'static str,
    extra_authorize_params: &'static [(&'static str, &'static str)],
}

struct PkceCodes {
    verifier: String,
    challenge: String,
}

struct PendingProviderOAuth {
    authorization: ProviderOAuthAuthorization,
    task: JoinHandle<Result<ProviderOAuthTokens, String>>,
}

#[derive(Default)]
pub struct ProviderOAuthCoordinator {
    pending: Mutex<HashMap<String, PendingProviderOAuth>>,
}

impl ProviderOAuthCoordinator {
    pub async fn start(&self, provider_name: &str) -> Result<ProviderOAuthAuthorization, String> {
        let config = oauth_config(provider_name)?;
        {
            let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
            if let Some(existing) = pending.remove(provider_name) {
                existing.task.abort();
            }
        }

        let listener = TcpListener::bind(("127.0.0.1", config.callback_port))
            .await
            .map_err(|e| format!("Failed to start OAuth callback server: {e}"))?;
        let pkce = generate_pkce();
        let state = random_token();
        let redirect_uri = redirect_uri(&config);
        let authorization = ProviderOAuthAuthorization {
            provider_name: config.provider_name.to_string(),
            authorization_url: authorize_url(&config, &redirect_uri, &pkce, &state)?,
            redirect_uri: redirect_uri.clone(),
            expires_in_seconds: CALLBACK_TIMEOUT.as_secs(),
        };
        let task = tokio::spawn(run_callback_flow(
            listener,
            config,
            redirect_uri,
            pkce,
            state,
        ));

        let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
        pending.insert(
            provider_name.to_string(),
            PendingProviderOAuth {
                authorization: authorization.clone(),
                task,
            },
        );
        Ok(authorization)
    }

    pub async fn complete(&self, provider_name: &str) -> Result<ProviderOAuthTokens, String> {
        let pending = {
            let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
            pending.remove(provider_name)
        }
        .ok_or_else(|| format!("No pending OAuth flow for provider {provider_name}"))?;

        pending.task.await.map_err(|e| e.to_string())?
    }

    pub fn pending_authorization(&self, provider_name: &str) -> Option<ProviderOAuthAuthorization> {
        self.pending.lock().ok().and_then(|pending| {
            pending
                .get(provider_name)
                .map(|flow| flow.authorization.clone())
        })
    }
}

fn oauth_config(provider_name: &str) -> Result<ProviderOAuthConfig, String> {
    match provider_name.to_ascii_lowercase().as_str() {
        "openai" => Ok(ProviderOAuthConfig {
            provider_name: "openai",
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann",
            issuer: "https://auth.openai.com",
            scopes: "openid profile email offline_access",
            callback_port: 1455,
            callback_path: "/auth/callback",
            extra_authorize_params: &[
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("originator", "openman"),
            ],
        }),
        _ => Err(format!(
            "OAuth is not supported for provider {provider_name}"
        )),
    }
}

async fn run_callback_flow(
    listener: TcpListener,
    config: ProviderOAuthConfig,
    redirect_uri: String,
    pkce: PkceCodes,
    state: String,
) -> Result<ProviderOAuthTokens, String> {
    tokio::time::timeout(CALLBACK_TIMEOUT, async move {
        loop {
            let (mut stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            match handle_callback_request(&mut stream, &config, &redirect_uri, &pkce, &state).await
            {
                CallbackRequestResult::Continue => continue,
                CallbackRequestResult::Done(result) => return result,
            }
        }
    })
    .await
    .map_err(|_| "OAuth callback timeout - authorization took too long".to_string())?
}

enum CallbackRequestResult {
    Continue,
    Done(Result<ProviderOAuthTokens, String>),
}

async fn handle_callback_request(
    stream: &mut TcpStream,
    config: &ProviderOAuthConfig,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> CallbackRequestResult {
    let request = match read_http_request(stream).await {
        Ok(request) => request,
        Err(error) => {
            let _ = write_html_response(stream, 400, &html_error(&error)).await;
            return CallbackRequestResult::Continue;
        }
    };
    let url = match reqwest::Url::parse(&format!(
        "http://localhost:{}{}",
        config.callback_port, request
    )) {
        Ok(url) => url,
        Err(error) => {
            let _ = write_html_response(stream, 400, &html_error(&error.to_string())).await;
            return CallbackRequestResult::Continue;
        }
    };

    if url.path() == "/cancel" {
        let _ = write_text_response(stream, 200, "OAuth login cancelled").await;
        return CallbackRequestResult::Done(Err("OAuth login cancelled".to_string()));
    }
    if url.path() != config.callback_path {
        let _ = write_text_response(stream, 404, "Not found").await;
        return CallbackRequestResult::Continue;
    }

    if let Some(error) = url.query_pairs().find(|(key, _)| key == "error") {
        let message = url
            .query_pairs()
            .find(|(key, _)| key == "error_description")
            .map(|(_, value)| value.to_string())
            .unwrap_or_else(|| error.1.to_string());
        let _ = write_html_response(stream, 200, &html_error(&message)).await;
        return CallbackRequestResult::Done(Err(message));
    }

    let Some(code) = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.to_string())
    else {
        let message = "Missing authorization code".to_string();
        let _ = write_html_response(stream, 400, &html_error(&message)).await;
        return CallbackRequestResult::Done(Err(message));
    };

    if url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.to_string())
        .as_deref()
        != Some(state)
    {
        let message = "Invalid OAuth state - potential CSRF attack".to_string();
        let _ = write_html_response(stream, 400, &html_error(&message)).await;
        return CallbackRequestResult::Done(Err(message));
    }

    let result = exchange_code_for_tokens(config, redirect_uri, pkce, &code).await;
    match &result {
        Ok(_) => {
            let _ = write_html_response(stream, 200, HTML_SUCCESS).await;
        }
        Err(error) => {
            let _ = write_html_response(stream, 500, &html_error(error)).await;
        }
    }
    CallbackRequestResult::Done(result)
}

async fn read_http_request(stream: &mut TcpStream) -> Result<String, String> {
    let mut buffer = [0_u8; 4096];
    let read = stream.read(&mut buffer).await.map_err(|e| e.to_string())?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| "Invalid OAuth callback request".to_string())?;
    let mut parts = request_line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Ok(target.to_string()),
        _ => Err("OAuth callback must be a GET request".to_string()),
    }
}

async fn exchange_code_for_tokens(
    config: &ProviderOAuthConfig,
    redirect_uri: &str,
    pkce: &PkceCodes,
    code: &str,
) -> Result<ProviderOAuthTokens, String> {
    let response = reqwest::Client::new()
        .post(format!("{}/oauth/token", config.issuer))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", config.client_id),
            ("code_verifier", pkce.verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Token exchange failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("Token exchange failed: {}", response.status()));
    }

    let tokens = response
        .json::<OpenAiTokenResponse>()
        .await
        .map_err(|e| format!("Token response was invalid: {e}"))?;
    Ok(ProviderOAuthTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
    })
}

#[derive(Deserialize)]
struct OpenAiTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

fn authorize_url(
    config: &ProviderOAuthConfig,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", config.issuer))
        .map_err(|e| e.to_string())?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", config.scopes)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        for (key, value) in config.extra_authorize_params {
            query.append_pair(key, value);
        }
    }
    Ok(url.to_string())
}

fn redirect_uri(config: &ProviderOAuthConfig) -> String {
    format!(
        "http://localhost:{}{}",
        config.callback_port, config.callback_path
    )
}

fn generate_pkce() -> PkceCodes {
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkceCodes {
        verifier,
        challenge,
    }
}

fn random_token() -> String {
    [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()]
        .into_iter()
        .map(|id| id.simple().to_string())
        .collect::<Vec<_>>()
        .join("")
}

async fn write_text_response(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
) -> Result<(), String> {
    write_response(stream, status, "text/plain; charset=utf-8", body).await
}

async fn write_html_response(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
) -> Result<(), String> {
    write_response(stream, status, "text/html; charset=utf-8", body).await
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<(), String> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .map_err(|e| e.to_string())
}

fn html_error(error: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
  <head>
    <title>Openman - Authorization Failed</title>
    <style>
      body {{ font-family: system-ui, -apple-system, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #131010; color: #f1ecec; }}
      .container {{ text-align: center; padding: 2rem; }}
      h1 {{ color: #fc533a; margin-bottom: 1rem; }}
      p {{ color: #b7b1b1; }}
      .error {{ color: #ff917b; font-family: monospace; margin-top: 1rem; padding: 1rem; background: #3c140d; border-radius: 0.5rem; }}
    </style>
  </head>
  <body>
    <div class="container">
      <h1>Authorization Failed</h1>
      <p>An error occurred during authorization.</p>
      <div class="error">{}</div>
    </div>
  </body>
</html>"#,
        escape_html(error)
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const HTML_SUCCESS: &str = r#"<!doctype html>
<html>
  <head>
    <title>Openman - Authorization Successful</title>
    <style>
      body { font-family: system-ui, -apple-system, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #131010; color: #f1ecec; }
      .container { text-align: center; padding: 2rem; }
      h1 { color: #f1ecec; margin-bottom: 1rem; }
      p { color: #b7b1b1; }
    </style>
  </head>
  <body>
    <div class="container">
      <h1>Authorization Successful</h1>
      <p>You can close this window and return to Openman.</p>
    </div>
    <script>setTimeout(() => window.close(), 2000)</script>
  </body>
</html>"#;
