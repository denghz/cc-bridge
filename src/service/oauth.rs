use crate::error::AppError;
use crate::tlsfp::make_request_client;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

#[derive(Debug, Clone)]
pub struct RefreshedOAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct OAuthRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// 通过轻量级 API 调用验证 Setup Token。
pub struct TokenTester;

impl TokenTester {
    pub fn new() -> Self {
        Self
    }

    /// 通过发送最小消息请求验证 Setup Token 有效性。
    /// When `gateway_url` is non-empty, the request is sent to the middleman
    /// gateway with `x-proxy-target-url` pointing to the real upstream.
    pub async fn test_token(&self, token: &str, proxy_url: &str, gateway_url: &str) -> Result<(), AppError> {
        let (request_url, upstream_url, use_gateway) =
            build_gateway_url("/v1/messages?beta=true", gateway_url);

        let body = serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "hi"}]
        });

        let client = make_request_client(proxy_url);

        let mut req = client
            .post(&request_url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", "claude-cli/2.1.89 (external, cli)")
            .header("x-app", "cli");
        if use_gateway {
            req = req.header("x-proxy-target-url", upstream_url);
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("request failed: {:?}", e)))?;

        if resp.status() != 200 {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "token test failed: status {} {}",
                status, text
            )));
        }
        Ok(())
    }
}

/// 使用 refresh token 刷新 OAuth access token。
pub async fn refresh_oauth_token(
    refresh_token: &str,
    proxy_url: &str,
) -> Result<RefreshedOAuthTokens, AppError> {
    let client = make_request_client(proxy_url);
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": OAUTH_CLIENT_ID,
        "scope": OAUTH_SCOPES.join(" "),
    });

    let resp = client
        .post(OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("oauth refresh request failed: {}", e)))?;

    if resp.status() != 200 {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "oauth refresh failed: status {} {}",
            status,
            text
        )));
    }

    let data: OAuthRefreshResponse = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("oauth refresh parse failed: {}", e)))?;

    let expires_in = if data.expires_in > 0 {
        data.expires_in
    } else {
        3600
    };
    let expires_at = Utc::now() + chrono::Duration::seconds(expires_in);

    Ok(RefreshedOAuthTokens {
        access_token: data.access_token,
        refresh_token: if data.refresh_token.is_empty() {
            refresh_token.to_string()
        } else {
            data.refresh_token
        },
        expires_at,
    })
}

/// 从 Anthropic OAuth API 获取账号用量数据。
/// When `gateway_url` is non-empty, the request is sent to the middleman
/// gateway with `x-proxy-target-url` pointing to the real upstream.
pub async fn fetch_usage(token: &str, proxy_url: &str, gateway_url: &str) -> Result<Value, AppError> {
    let (request_url, upstream_url, use_gateway) =
        build_gateway_url("/api/oauth/usage", gateway_url);
    let client = make_request_client(proxy_url);

    let mut req = client
        .get(&request_url);
    if use_gateway {
        req = req.header("x-proxy-target-url", upstream_url);
    }
    let resp = req
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", "claude-code/2.1.89 (external, cli)")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("usage request failed: {}", e)))?;

    let status = resp.status();
    if status != 200 {
        let text = resp.text().await.unwrap_or_default();
        return Err(match status.as_u16() {
            401 | 403 => AppError::BadRequest(format!(
                "usage fetch failed: status {} — token may be expired or invalid: {}",
                status, text
            )),
            429 => AppError::TooManyRequests(format!(
                "usage endpoint rate limited (429), try again later: {}",
                text
            )),
            _ => AppError::Internal(format!(
                "usage fetch failed: status {} {}",
                status, text
            )),
        });
    }

    let data: Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("usage parse failed: {}", e)))?;
    Ok(data)
}

const UPSTREAM_BASE: &str = "https://api.anthropic.com";

/// Compute the HTTP request URL for a given path, optionally going through a
/// middleman gateway.
///
/// Returns `(request_url, upstream_url, use_gateway)`.
/// - When `gateway_url` is empty: `request_url == upstream_url`, direct mode.
/// - When set: `request_url` targets the gateway, `upstream_url` is the
///   canonical Anthropic URL that must be sent in `x-proxy-target-url`.
fn build_gateway_url(path: &str, gateway_url: &str) -> (String, String, bool) {
    let upstream_url = format!("{}{}", UPSTREAM_BASE, path);
    if gateway_url.is_empty() {
        (upstream_url.clone(), upstream_url, false)
    } else {
        let gw = gateway_url.trim_end_matches('/');
        let request_url = format!("{}{}", gw, path);
        (request_url, upstream_url, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // build_gateway_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_gateway_url_empty_is_direct() {
        let (req, upstream, gw) = build_gateway_url("/v1/messages?beta=true", "");
        assert_eq!(req, "https://api.anthropic.com/v1/messages?beta=true");
        assert_eq!(upstream, req);
        assert!(!gw);
    }

    #[test]
    fn test_build_gateway_url_empty_usage() {
        let (req, upstream, gw) = build_gateway_url("/api/oauth/usage", "");
        assert_eq!(req, "https://api.anthropic.com/api/oauth/usage");
        assert_eq!(upstream, req);
        assert!(!gw);
    }

    #[test]
    fn test_build_gateway_url_with_gateway() {
        let (req, upstream, gw) =
            build_gateway_url("/v1/messages?beta=true", "http://gw.example.com");
        assert_eq!(req, "http://gw.example.com/v1/messages?beta=true");
        assert_eq!(
            upstream,
            "https://api.anthropic.com/v1/messages?beta=true"
        );
        assert!(gw);
    }

    #[test]
    fn test_build_gateway_url_trailing_slash_stripped() {
        let (req, _, gw) =
            build_gateway_url("/api/oauth/usage", "http://gw.example.com/");
        assert_eq!(req, "http://gw.example.com/api/oauth/usage");
        assert!(gw);
    }
}
