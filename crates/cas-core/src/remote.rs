use crate::auth::{auth_last_refresh_millis, identities_match, validate_auth};
use crate::{
    AuthIdentity, BrowserLogin, CasError, DeviceLogin, Result, UsageWindow, ui_is_chinese,
};
use base64::Engine as _;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const DEFAULT_AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEFAULT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const DEFAULT_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const DEFAULT_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const TEST_MODEL: &str = "gpt-6-luna";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_AFTER: Duration = Duration::from_secs(8 * 24 * 60 * 60);
const REFRESH_EARLY: i64 = 5 * 60;
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const BROWSER_LOGIN_PORT: u16 = 1455;
const BROWSER_LOGIN_FALLBACK_PORT: u16 = 1457;

static HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct AccountProbe {
    pub valid: Option<bool>,
    pub plan_type: Option<String>,
    pub five_hour: Option<UsageWindow>,
    pub long_window: Option<UsageWindow>,
    pub error: Option<String>,
    pub auth_bytes: Option<Vec<u8>>,
}

#[derive(Debug)]
pub(crate) struct AccountTestProbe {
    pub reasoning_effort: String,
    pub response: Option<String>,
    pub error: Option<String>,
    pub request_headers: Vec<(String, String)>,
    pub auth_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct Credentials {
    root: Value,
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

pub(crate) fn probe_account(auth_bytes: &[u8]) -> Result<AccountProbe> {
    let client = http_client()?;
    let original_identity = validate_auth(auth_bytes)?;
    let mut current_bytes = auth_bytes.to_vec();
    let mut credentials = parse_credentials(&current_bytes)?;
    let mut refreshed = false;

    if should_refresh(&current_bytes, &credentials.access_token)
        && credentials.refresh_token.is_some()
    {
        current_bytes = refresh_auth(client, &credentials)?;
        ensure_same_identity(&original_identity, &current_bytes)?;
        credentials = parse_credentials(&current_bytes)?;
        refreshed = true;
    }

    let mut response = usage_request(client, &credentials)?;
    if response.status() == StatusCode::UNAUTHORIZED && !refreshed {
        if credentials.refresh_token.is_none() {
            return Ok(invalid_probe(
                "usage API rejected the credential and no refresh token is available",
                None,
            ));
        }
        current_bytes = refresh_auth(client, &credentials)?;
        ensure_same_identity(&original_identity, &current_bytes)?;
        credentials = parse_credentials(&current_bytes)?;
        refreshed = true;
        response = usage_request(client, &credentials)?;
    }

    if !response.status().is_success() {
        let status = response.status();
        let detail = response_detail(response);
        let message = format!("usage API returned HTTP {status}{detail}");
        return Ok(AccountProbe {
            valid: if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                Some(false)
            } else {
                None
            },
            plan_type: None,
            five_hour: None,
            long_window: None,
            error: Some(message),
            auth_bytes: refreshed.then_some(current_bytes),
        });
    }

    let payload: Value = response.json()?;
    let plan_type = payload
        .get("plan_type")
        .or_else(|| payload.get("planType"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (five_hour, long_window) = parse_usage_windows(&payload);

    Ok(AccountProbe {
        valid: Some(true),
        plan_type,
        five_hour,
        long_window,
        error: None,
        auth_bytes: refreshed.then_some(current_bytes),
    })
}

pub(crate) fn test_account_streaming(auth_bytes: &[u8]) -> Result<AccountTestProbe> {
    let client = http_client()?;
    let original_identity = validate_auth(auth_bytes)?;
    let mut current_bytes = auth_bytes.to_vec();
    let mut credentials = parse_credentials(&current_bytes)?;
    let mut refreshed = false;

    if should_refresh(&current_bytes, &credentials.access_token)
        && credentials.refresh_token.is_some()
    {
        current_bytes = refresh_auth(client, &credentials)?;
        ensure_same_identity(&original_identity, &current_bytes)?;
        credentials = parse_credentials(&current_bytes)?;
        refreshed = true;
    }

    let mut effort = lowest_luna_effort(client, &credentials).unwrap_or_else(|| "low".into());
    let mut attempt = send_luna_hello(client, &credentials, &effort);

    if attempt.status == Some(StatusCode::UNAUTHORIZED)
        && !refreshed
        && credentials.refresh_token.is_some()
    {
        current_bytes = refresh_auth(client, &credentials)?;
        ensure_same_identity(&original_identity, &current_bytes)?;
        credentials = parse_credentials(&current_bytes)?;
        refreshed = true;
        effort = lowest_luna_effort(client, &credentials).unwrap_or_else(|| "low".into());
        attempt = send_luna_hello(client, &credentials, &effort);
    }

    Ok(AccountTestProbe {
        reasoning_effort: effort,
        response: attempt.response,
        error: attempt.error,
        request_headers: attempt.request_headers,
        auth_bytes: refreshed.then_some(current_bytes),
    })
}

#[derive(Debug)]
struct ModelAttempt {
    status: Option<StatusCode>,
    response: Option<String>,
    error: Option<String>,
    request_headers: Vec<(String, String)>,
}

fn lowest_luna_effort(client: &Client, credentials: &Credentials) -> Option<String> {
    let mut url = reqwest::Url::parse(&models_url()).ok()?;
    if !url.query_pairs().any(|(key, _)| key == "client_version") {
        url.query_pairs_mut()
            .append_pair("client_version", env!("CARGO_PKG_VERSION"));
    }
    let mut request = client
        .get(url)
        .bearer_auth(&credentials.access_token)
        .header("Accept", "application/json")
        .header("originator", "codex_cli_rs");
    if let Some(account_id) = credentials.account_id.as_deref() {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    let response = request.send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload: Value = response.json().ok()?;
    let model = payload
        .get("models")?
        .as_array()?
        .iter()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some(TEST_MODEL))?;
    let levels = model
        .get("supported_reasoning_levels")
        .or_else(|| model.get("supported_reasoning_efforts"))?
        .as_array()?;
    let advertised: Vec<&str> = levels
        .iter()
        .filter_map(|level| {
            level
                .get("effort")
                .and_then(Value::as_str)
                .or_else(|| level.as_str())
        })
        .collect();
    [
        "none",
        "minimal",
        "low",
        "medium",
        "high",
        "xhigh",
        "max",
        "ultra",
        "persistent",
    ]
    .into_iter()
    .find(|candidate| advertised.iter().any(|effort| effort == candidate))
    .map(str::to_owned)
}

fn send_luna_hello(client: &Client, credentials: &Credentials, effort: &str) -> ModelAttempt {
    let body = json!({
        "model": TEST_MODEL,
        "stream": true,
        "instructions": "",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": effort},
        "store": false,
        "include": []
    });

    let mut builder = client
        .post(responses_url())
        .bearer_auth(&credentials.access_token)
        .header("Accept", "text/event-stream")
        .header("originator", "codex_cli_rs")
        .header("User-Agent", concat!("cas/", env!("CARGO_PKG_VERSION")))
        .json(&body);
    if let Some(account_id) = credentials.account_id.as_deref() {
        builder = builder.header("ChatGPT-Account-ID", account_id);
    }

    let request = match builder.build() {
        Ok(request) => request,
        Err(error) => {
            return ModelAttempt {
                status: None,
                response: None,
                error: Some(format!("failed to build model request: {error}")),
                request_headers: Vec::new(),
            };
        }
    };
    let request_headers = display_request_headers(request.headers());
    let response = match client.execute(request) {
        Ok(response) => response,
        Err(error) => {
            return ModelAttempt {
                status: None,
                response: None,
                error: Some(format!("model request failed: {error}")),
                request_headers,
            };
        }
    };
    let status = response.status();
    if !status.is_success() {
        let detail = response_detail(response);
        return ModelAttempt {
            status: Some(status),
            response: None,
            error: Some(format!("model API returned HTTP {status}{detail}")),
            request_headers,
        };
    }

    match read_streaming_text(response) {
        Ok(text) => ModelAttempt {
            status: Some(status),
            response: Some(text),
            error: None,
            request_headers,
        },
        Err(error) => ModelAttempt {
            status: Some(status),
            response: None,
            error: Some(error),
            request_headers,
        },
    }
}

fn read_streaming_text(response: Response) -> std::result::Result<String, String> {
    let mut reader = BufReader::new(response);
    let mut line = String::new();
    let mut output = String::new();
    let mut completed = false;

    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("failed while reading model stream: {error}"))?;
        if read == 0 {
            break;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|error| format!("invalid model stream event: {error}"))?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    output.push_str(delta);
                }
            }
            Some("response.completed") => {
                completed = true;
                if output.is_empty()
                    && let Some(text) = completed_response_text(&event)
                {
                    output.push_str(&text);
                }
            }
            Some("response.failed") | Some("response.incomplete") => {
                let detail = event
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| event.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("model stream ended unsuccessfully");
                return Err(detail.to_owned());
            }
            Some("error") => {
                let detail = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| event.get("message").and_then(Value::as_str))
                    .unwrap_or("model stream returned an error event");
                return Err(detail.to_owned());
            }
            _ => {}
        }
    }

    if completed {
        Ok(output)
    } else {
        Err("model stream ended before response.completed".into())
    }
}

fn completed_response_text(event: &Value) -> Option<String> {
    let output = event.pointer("/response/output")?.as_array()?;
    let mut text = String::new();
    for item in output {
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(Value::as_str) == Some("output_text")
                && let Some(value) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(value);
            }
        }
    }
    (!text.is_empty()).then_some(text)
}

fn display_request_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name_text = name.as_str().to_owned();
            let value_text = if matches!(
                name.as_str(),
                "authorization" | "cookie" | "x-gateway-auth" | "proxy-authorization"
            ) {
                "<redacted>".into()
            } else {
                value
                    .to_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|_| "<non-utf8>".into())
            };
            (name_text, value_text)
        })
        .collect()
}

pub(crate) fn begin_device_login() -> Result<DeviceLogin> {
    let client = http_client()?;
    let auth_base = auth_base_url();
    let response = client
        .post(format!("{auth_base}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": oauth_client_id()}))
        .send()?;
    if !response.status().is_success() {
        return Err(remote_response_error("device login start", response));
    }
    let payload: Value = response.json()?;
    let device_auth_id = required_string(&payload, "device_auth_id")?;
    let user_code = payload
        .get("user_code")
        .or_else(|| payload.get("usercode"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| CasError::Remote("device login response omitted user_code".into()))?;
    let interval_secs = payload
        .get("interval")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .unwrap_or(5)
        .max(1);

    Ok(DeviceLogin {
        verification_url: format!("{auth_base}/codex/device"),
        user_code,
        device_auth_id,
        interval_secs,
    })
}

pub(crate) fn begin_browser_login() -> Result<BrowserLogin> {
    let listener = TcpListener::bind(("127.0.0.1", BROWSER_LOGIN_PORT))
        .or_else(|_| TcpListener::bind(("127.0.0.1", BROWSER_LOGIN_FALLBACK_PORT)))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
    let code_verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(code_verifier.as_bytes()));
    let state = Uuid::new_v4().to_string();
    let auth_base = auth_base_url();
    let mut url = reqwest::Url::parse(&format!("{auth_base}/oauth/authorize"))
        .map_err(|error| CasError::Remote(format!("failed to build browser login URL: {error}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &oauth_client_id())
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair(
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        )
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "codex_cli_rs");

    Ok(BrowserLogin {
        auth_url: url.into(),
        redirect_uri,
        code_verifier,
        state,
        listener,
    })
}

pub(crate) fn complete_browser_login(login: BrowserLogin) -> Result<Vec<u8>> {
    let started = Instant::now();
    loop {
        match login.listener.accept() {
            Ok((mut stream, _)) => {
                if let Some(result) = handle_browser_callback(&mut stream, &login) {
                    return result;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= BROWSER_LOGIN_TIMEOUT {
                    return Err(CasError::Remote(
                        "browser login timed out after 15 minutes".into(),
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn handle_browser_callback(
    stream: &mut TcpStream,
    login: &BrowserLogin,
) -> Option<Result<Vec<u8>>> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buffer = [0u8; 16 * 1024];
    let read = match stream.read(&mut buffer) {
        Ok(read) if read > 0 => read,
        _ => return None,
    };
    let request = String::from_utf8_lossy(&buffer[..read]);
    let Some(target) = request.lines().next().and_then(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some("GET"))
            .then(|| parts.next())
            .flatten()
    }) else {
        let _ = browser_response(
            stream,
            400,
            if ui_is_chinese() {
                "请求无效"
            } else {
                "Bad Request"
            },
        );
        return None;
    };
    let url = match reqwest::Url::parse(&format!("http://localhost{target}")) {
        Ok(url) => url,
        Err(_) => {
            let _ = browser_response(
                stream,
                400,
                if ui_is_chinese() {
                    "请求无效"
                } else {
                    "Bad Request"
                },
            );
            return None;
        }
    };
    if url.path() != "/auth/callback" {
        let _ = browser_response(
            stream,
            404,
            if ui_is_chinese() {
                "页面不存在"
            } else {
                "Not Found"
            },
        );
        return None;
    }

    let mut code = None;
    let mut state = None;
    let mut provider_error = None;
    let mut provider_error_description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => provider_error = Some(value.into_owned()),
            "error_description" => provider_error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(login.state.as_str()) {
        let _ = browser_response(
            stream,
            400,
            if ui_is_chinese() {
                "登录失败：状态校验不匹配"
            } else {
                "Login failed: state mismatch"
            },
        );
        return Some(Err(CasError::Remote(
            "browser login callback state mismatch".into(),
        )));
    }
    if let Some(error) = provider_error {
        let detail = provider_error_description
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!(": {value}"))
            .unwrap_or_default();
        let _ = browser_response(
            stream,
            400,
            if ui_is_chinese() {
                "登录失败，请返回终端。"
            } else {
                "Login failed. Return to the terminal."
            },
        );
        return Some(Err(CasError::Remote(format!(
            "browser login failed: {error}{detail}"
        ))));
    }
    let Some(code) = code.filter(|value| !value.is_empty()) else {
        let _ = browser_response(
            stream,
            400,
            if ui_is_chinese() {
                "登录失败：缺少授权码"
            } else {
                "Login failed: missing authorization code"
            },
        );
        return Some(Err(CasError::Remote(
            "browser login callback omitted authorization code".into(),
        )));
    };

    let result = exchange_browser_code(login, &code);
    match &result {
        Ok(_) => {
            let _ = browser_response(
                stream,
                200,
                if ui_is_chinese() {
                    "登录完成，可以返回 CAS。"
                } else {
                    "Login complete. You can return to CAS."
                },
            );
        }
        Err(_) => {
            let _ = browser_response(
                stream,
                500,
                if ui_is_chinese() {
                    "登录失败，请返回终端。"
                } else {
                    "Login failed. Return to the terminal."
                },
            );
        }
    }
    Some(result)
}

fn browser_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let html = browser_response_html(body);
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    )?;
    stream.flush()
}

fn browser_response_html(body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"color-scheme\" content=\"dark\"><title>CAS Login</title><style>html,body{{margin:0;min-height:100%;background:#000;color:#fff}}body{{min-height:100vh;display:grid;place-items:center;font:16px/1.5 system-ui,-apple-system,BlinkMacSystemFont,\"Segoe UI\",sans-serif}}main{{max-width:42rem;padding:2rem;text-align:center}}</style></head><body><main>{body}</main></body></html>"
    )
}

fn exchange_browser_code(login: &BrowserLogin, code: &str) -> Result<Vec<u8>> {
    let client = http_client()?;
    let auth_base = auth_base_url();
    let response = client
        .post(format!("{auth_base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", oauth_client_id().as_str()),
            ("code", code),
            ("redirect_uri", login.redirect_uri.as_str()),
            ("code_verifier", login.code_verifier.as_str()),
        ])
        .send()?;
    if !response.status().is_success() {
        return Err(remote_response_error(
            "browser login token exchange",
            response,
        ));
    }
    let payload: Value = response.json()?;
    build_auth_json(
        required_string(&payload, "id_token")?,
        required_string(&payload, "access_token")?,
        required_string(&payload, "refresh_token")?,
    )
}

pub(crate) fn complete_device_login(login: DeviceLogin) -> Result<Vec<u8>> {
    let client = http_client()?;
    let auth_base = auth_base_url();
    let poll_url = format!("{auth_base}/api/accounts/deviceauth/token");
    let started = Instant::now();

    let (authorization_code, code_verifier) = loop {
        let response = client
            .post(&poll_url)
            .json(&json!({
                "device_auth_id": login.device_auth_id,
                "user_code": login.user_code,
            }))
            .send()?;
        if response.status().is_success() {
            let payload: Value = response.json()?;
            break (
                required_string(&payload, "authorization_code")?,
                required_string(&payload, "code_verifier")?,
            );
        }
        if !matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Err(remote_response_error("device login poll", response));
        }
        if started.elapsed() >= DEVICE_LOGIN_TIMEOUT {
            return Err(CasError::Remote(
                "device login timed out after 15 minutes".into(),
            ));
        }
        std::thread::sleep(Duration::from_secs(login.interval_secs));
    };

    let redirect_uri = format!("{auth_base}/deviceauth/callback");
    let response = client
        .post(format!("{auth_base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", oauth_client_id().as_str()),
            ("code", authorization_code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", code_verifier.as_str()),
        ])
        .send()?;
    if !response.status().is_success() {
        return Err(remote_response_error(
            "device login token exchange",
            response,
        ));
    }
    let payload: Value = response.json()?;
    let id_token = required_string(&payload, "id_token")?;
    let access_token = required_string(&payload, "access_token")?;
    let refresh_token = required_string(&payload, "refresh_token")?;
    build_auth_json(id_token, access_token, refresh_token)
}

pub fn initialize_http_client() -> Result<()> {
    if HTTP_CLIENT.get().is_some() {
        return Ok(());
    }
    let client = build_http_client()?;
    let _ = HTTP_CLIENT.set(client);
    Ok(())
}

fn build_http_client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("cas/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn http_client() -> Result<&'static Client> {
    initialize_http_client()?;
    HTTP_CLIENT.get().ok_or_else(|| {
        CasError::Verification("HTTP client was not initialized after successful startup".into())
    })
}

fn auth_base_url() -> String {
    std::env::var("CAS_AUTH_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AUTH_BASE_URL.into())
        .trim_end_matches('/')
        .to_owned()
}

fn usage_url() -> String {
    std::env::var("CAS_USAGE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_USAGE_URL.into())
}

fn models_url() -> String {
    std::env::var("CAS_MODELS_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS_URL.into())
}

fn responses_url() -> String {
    std::env::var("CAS_RESPONSES_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RESPONSES_URL.into())
}

fn oauth_client_id() -> String {
    std::env::var("CAS_OAUTH_CLIENT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.into())
}

fn parse_credentials(bytes: &[u8]) -> Result<Credentials> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let root: Value = serde_json::from_slice(bytes)?;
    let tokens = root
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            CasError::InvalidAuth("credential does not contain a tokens object".into())
        })?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            CasError::InvalidAuth("credential does not contain tokens.access_token".into())
        })?;
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            validate_auth(bytes)
                .ok()
                .and_then(|identity| identity.account_id)
        });
    Ok(Credentials {
        root,
        access_token,
        refresh_token,
        account_id,
    })
}

fn should_refresh(bytes: &[u8], access_token: &str) -> bool {
    let now = chrono::Utc::now().timestamp();
    if jwt_expiration(access_token).is_some_and(|expiry| expiry <= now + REFRESH_EARLY) {
        return true;
    }
    let Some(last_refresh) = auth_last_refresh_millis(bytes) else {
        return false;
    };
    chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(last_refresh)
        >= REFRESH_AFTER.as_millis() as i64
}

fn jwt_expiration(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice::<Value>(&decoded)
        .ok()?
        .get("exp")?
        .as_i64()
}

fn refresh_auth(client: &Client, credentials: &Credentials) -> Result<Vec<u8>> {
    let refresh_token = credentials.refresh_token.as_deref().ok_or_else(|| {
        CasError::InvalidAuth("credential does not contain a refresh token".into())
    })?;
    let response = client
        .post(format!("{}/oauth/token", auth_base_url()))
        .form(&[
            ("client_id", oauth_client_id().as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()?;
    if !response.status().is_success() {
        return Err(remote_response_error("token refresh", response));
    }
    let refreshed: Value = response.json()?;
    let mut root = credentials.root.clone();
    let tokens = root
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            CasError::InvalidAuth("credential does not contain a tokens object".into())
        })?;
    for key in ["id_token", "access_token", "refresh_token"] {
        if let Some(value) = refreshed.get(key).and_then(Value::as_str) {
            tokens.insert(key.into(), Value::String(value.to_owned()));
        }
    }
    root.as_object_mut()
        .ok_or_else(|| CasError::InvalidAuth("auth.json must contain a JSON object".into()))?
        .insert(
            "last_refresh".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
    Ok(serde_json::to_vec_pretty(&root)?)
}

fn usage_request(client: &Client, credentials: &Credentials) -> Result<Response> {
    let mut request = client
        .get(usage_url())
        .bearer_auth(&credentials.access_token)
        .header("Accept", "application/json");
    if let Some(account_id) = credentials.account_id.as_deref() {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    Ok(request.send()?)
}

fn ensure_same_identity(before: &AuthIdentity, refreshed: &[u8]) -> Result<()> {
    let after = validate_auth(refreshed)?;
    if before.has_stable_field() && after.has_stable_field() && !identities_match(before, &after) {
        return Err(CasError::Verification(
            "token refresh returned credentials for a different account".into(),
        ));
    }
    Ok(())
}

fn build_auth_json(
    id_token: String,
    access_token: String,
    refresh_token: String,
) -> Result<Vec<u8>> {
    let mut root = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": refresh_token,
        },
        "last_refresh": chrono::Utc::now().to_rfc3339(),
    });
    let provisional = serde_json::to_vec(&root)?;
    let identity = validate_auth(&provisional)?;
    if !identity.has_stable_field() {
        return Err(CasError::InvalidAuth(
            "login token did not contain a stable account identity".into(),
        ));
    }
    if let Some(account_id) = identity.account_id {
        root["tokens"]["account_id"] = Value::String(account_id);
    }
    Ok(serde_json::to_vec_pretty(&root)?)
}

fn parse_usage_windows(payload: &Value) -> (Option<UsageWindow>, Option<UsageWindow>) {
    let limits = payload
        .get("rate_limit")
        .or_else(|| payload.get("rateLimit"));
    let Some(limits) = limits else {
        return (None, None);
    };
    let mut windows = Vec::new();
    for key in [
        "primary_window",
        "secondary_window",
        "primaryWindow",
        "secondaryWindow",
        "primary",
        "secondary",
    ] {
        if let Some(value) = limits.get(key).filter(|value| !value.is_null())
            && let Some(window) = parse_window(value)
            && !windows.iter().any(|existing: &UsageWindow| {
                existing.window_duration_mins == window.window_duration_mins
                    && existing.resets_at == window.resets_at
            })
        {
            windows.push(window);
        }
    }
    let five_hour = windows
        .iter()
        .find(|window| window.window_duration_mins == Some(300))
        .cloned();
    let long_window = windows
        .into_iter()
        .filter(|window| window.window_duration_mins != Some(300))
        .max_by_key(|window| window.window_duration_mins.unwrap_or_default());
    (five_hour, long_window)
}

fn parse_window(value: &Value) -> Option<UsageWindow> {
    let used_percent = value
        .get("used_percent")
        .or_else(|| value.get("usedPercent"))
        .and_then(number_as_i32)?;
    let resets_at = value
        .get("reset_at")
        .or_else(|| value.get("resets_at"))
        .or_else(|| value.get("resetsAt"))
        .and_then(number_as_i64);
    let duration_seconds = value
        .get("limit_window_seconds")
        .or_else(|| value.get("limitWindowSeconds"))
        .and_then(number_as_i64);
    let duration_mins = duration_seconds
        .map(|seconds| seconds / 60)
        .or_else(|| value.get("windowDurationMins").and_then(number_as_i64));
    Some(UsageWindow {
        used_percent,
        remaining_percent: (100 - used_percent).clamp(0, 100),
        resets_at,
        window_duration_mins: duration_mins,
    })
}

fn number_as_i32(value: &Value) -> Option<i32> {
    value
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .or_else(|| value.as_f64().map(|value| value.round() as i32))
}

fn number_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_f64().map(|value| value.round() as i64))
}

fn required_string(payload: &Value, key: &str) -> Result<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CasError::Remote(format!("remote response omitted {key}")))
}

fn response_detail(response: Response) -> String {
    let payload = response.json::<Value>().ok();
    let detail = payload.as_ref().and_then(|value| {
        value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
            .or_else(|| value.get("detail").and_then(Value::as_str))
            .or_else(|| value.get("message").and_then(Value::as_str))
    });
    detail
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default()
}

fn remote_response_error(operation: &str, response: Response) -> CasError {
    let status = response.status();
    let detail = response_detail(response);
    CasError::Remote(format!("{operation} returned HTTP {status}{detail}"))
}

fn invalid_probe(message: &str, auth_bytes: Option<Vec<u8>>) -> AccountProbe {
    AccountProbe {
        valid: Some(false),
        plan_type: None,
        five_hour: None,
        long_window: None,
        error: Some(message.into()),
        auth_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_by_duration_not_position() {
        let payload = json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 41.4,
                    "limit_window_seconds": 604800,
                    "reset_at": 20
                },
                "secondary_window": {
                    "used_percent": 23,
                    "limit_window_seconds": 18000,
                    "reset_at": 10
                }
            }
        });
        let (five, long) = parse_usage_windows(&payload);
        assert_eq!(five.unwrap().remaining_percent, 77);
        let long = long.unwrap();
        assert_eq!(long.remaining_percent, 59);
        assert_eq!(long.label(), "week");
    }

    #[test]
    fn handles_missing_five_hour_window() {
        let payload = json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 2592000
                },
                "secondary_window": null
            }
        });
        let (five, long) = parse_usage_windows(&payload);
        assert!(five.is_none());
        assert_eq!(long.unwrap().label(), "30d");
    }

    #[test]
    fn browser_callback_page_is_dark() {
        let html = browser_response_html("Login complete.");
        assert!(html.contains("background:#000"));
        assert!(html.contains("color:#fff"));
        assert!(html.contains("color-scheme\" content=\"dark"));
        assert!(html.contains("Login complete."));
    }
}
