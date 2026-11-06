//! Cloudflare Turnstile CAPTCHA (human check) for sign-in screens.
//!
//! Optional: it is enforced only when BOTH env vars are set:
//!   TURNSTILE_SITE_KEY    public key, shown to the browser
//!   TURNSTILE_SECRET_KEY  secret key, used server-side to verify tokens
//! With either missing, `site_key()` is `None` and `verify()` always passes, so
//! offline/.exe setups keep working with no CAPTCHA.

use crate::web::ApiError;
use serde::Deserialize;

const VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Public site key, only if the CAPTCHA is fully configured.
pub fn site_key() -> Option<String> {
    env("TURNSTILE_SECRET_KEY")?;
    env("TURNSTILE_SITE_KEY")
}

#[derive(Deserialize)]
struct Verdict {
    success: bool,
}

/// Verify the token the browser got from the Turnstile widget.
/// Passes immediately when the CAPTCHA isn't configured.
pub async fn verify(token: Option<&str>, remote_ip: Option<&str>) -> Result<(), ApiError> {
    let Some(secret) = env("TURNSTILE_SECRET_KEY") else {
        return Ok(());
    };
    if site_key().is_none() {
        return Ok(());
    }
    let token = token.map(str::trim).filter(|t| !t.is_empty() && t.len() <= 2048);
    let Some(token) = token else {
        return Err(ApiError::bad_request(
            "Please complete the “I'm not a robot” check",
        ));
    };

    let mut form = vec![("secret", secret), ("response", token.to_string())];
    if let Some(ip) = remote_ip {
        form.push(("remoteip", ip.to_string()));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let verdict: Verdict = client
        .post(VERIFY_URL)
        .form(&form)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| {
            eprintln!("[captcha] verify request failed: {e}");
            ApiError::internal("Couldn't verify the CAPTCHA right now. Please try again.")
        })?
        .json()
        .await
        .map_err(|e| {
            eprintln!("[captcha] bad verify response: {e}");
            ApiError::internal("Couldn't verify the CAPTCHA right now. Please try again.")
        })?;

    if verdict.success {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "CAPTCHA check failed. Please try again.",
        ))
    }
}

/// Client IP as seen behind Vercel's proxy (first `x-forwarded-for` entry).
pub fn client_ip(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
