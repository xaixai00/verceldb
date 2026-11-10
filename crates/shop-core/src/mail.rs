//! Transactional email through Brevo (https://www.brevo.com).
//!
//! Used for password-reset codes. Configured with env vars:
//!   BREVO_API_KEY    API key (Brevo → SMTP & API → API Keys)
//!   MAIL_FROM        a sender address you have verified in Brevo
//!   MAIL_FROM_NAME   optional display name (default: the shop name)
//!
//! With BREVO_API_KEY or MAIL_FROM missing, `configured()` is false and the
//! "Forgot password" option is hidden.

use crate::web::ApiError;
use serde_json::json;

const SEND_URL: &str = "https://api.brevo.com/v3/smtp/email";

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// True when both the API key and the sender address are set.
pub fn configured() -> bool {
    env("BREVO_API_KEY").is_some() && env("MAIL_FROM").is_some()
}

/// Minimal HTML escaping for text placed inside the email body.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Send a password-reset code. `minutes` is how long the code stays valid.
pub async fn send_reset_code(
    to_email: &str,
    to_name: &str,
    shop_name: &str,
    code: &str,
    minutes: i64,
) -> Result<(), ApiError> {
    let (Some(key), Some(from)) = (env("BREVO_API_KEY"), env("MAIL_FROM")) else {
        return Err(ApiError::internal(
            "Password reset isn't set up yet (missing BREVO_API_KEY or MAIL_FROM).",
        ));
    };
    let from_name = env("MAIL_FROM_NAME").unwrap_or_else(|| shop_name.to_string());
    let shop = esc(shop_name);

    let html = format!(
        "<div style=\"font-family:Arial,Helvetica,sans-serif;max-width:480px;margin:0 auto;padding:24px;color:#111\">\
         <h2 style=\"margin:0 0 12px\">{shop}</h2>\
         <p>Hi {name},</p>\
         <p>Use this code to reset your password:</p>\
         <p style=\"font-size:34px;letter-spacing:8px;font-weight:700;margin:18px 0\">{code}</p>\
         <p>It expires in {minutes} minutes. If you didn't ask for this, you can ignore this email — your password won't change.</p>\
         </div>",
        name = esc(to_name),
    );
    let text = format!(
        "Hi {to_name},\n\nYour {shop_name} password reset code is: {code}\n\n\
         It expires in {minutes} minutes. If you didn't ask for this, ignore this email.\n"
    );

    let body = json!({
        "sender": { "name": from_name, "email": from },
        "to": [{ "email": to_email, "name": to_name }],
        "subject": format!("{code} is your {shop_name} password reset code"),
        "htmlContent": html,
        "textContent": text,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    // BREVO_API_URL is only for tests (point it at a fake server).
    let url = env("BREVO_API_URL").unwrap_or_else(|| SEND_URL.to_string());
    let resp = client
        .post(url)
        .header("api-key", key)
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            eprintln!("[mail] request failed: {e}");
            ApiError::internal("Couldn't send the email right now. Please try again.")
        })?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        eprintln!("[mail] Brevo returned {status}: {detail}");
        Err(ApiError::internal(
            "Couldn't send the email right now. Please try again.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(esc("<b>&\"x\""), "&lt;b&gt;&amp;&quot;x&quot;");
    }
}
