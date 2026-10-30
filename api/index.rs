//! Vercel function: the whole JCS MotorParts site in one handler.
//!
//!   /          customer store
//!   /admin     admin (password from ADMIN_PASSWORD)
//!
//! `vercel.json` rewrites every path to this function.
//!
//! Environment variables (Vercel → Project → Settings → Environment Variables):
//!   TURSO_DATABASE_URL  libsql://<db>-<org>.turso.io
//!   TURSO_AUTH_TOKEN    database token from `turso db tokens create <db>`
//!   ADMIN_PASSWORD      password for /admin
//!
//! Outside Vercel this binary listens on 127.0.0.1:3000 (or $VERCEL_DEV_PORT),
//! which is handy for testing: `cargo run --bin index --features local`.

use axum::{
    body::Body,
    http::{header, Request, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use shop_admin::AdminConfig;
use shop_core::{web, Db};
use shop_customer::CustomerState;
use tokio::sync::OnceCell;
use tower::ServiceExt;
use vercel_runtime::{axum::VercelLayer, Error};

/// Built once per function instance; the first request also creates tables if needed.
static APP: OnceCell<Router> = OnceCell::const_new();

async fn build_app() -> Result<Router, String> {
    let db = Db::from_env().await.map_err(|e| e.to_string())?;
    let secure = web::on_vercel();
    let password = std::env::var("ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());

    let customer = shop_customer::router(CustomerState {
        db: db.clone(),
        secure_cookies: secure,
    });
    let admin = shop_admin::router(
        db,
        AdminConfig {
            base: "/admin".into(),
            password,
            local_only: false,
            secure_cookies: secure,
            store_url: "/".into(),
        },
    );
    Ok(customer
        .nest("/admin", admin)
        .route("/admin/", get(|| async { Redirect::permanent("/admin") })))
}

fn setup_error(msg: &str) -> Response {
    eprintln!("[startup] {msg}");
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Setup needed</title>\
         <body style=\"font:16px system-ui;background:#111214;color:#eceae6;padding:40px;max-width:640px\">\
         <h1 style=\"color:#ff6b1a\">JCS MotorParts can't start yet</h1>\
         <p>{}</p><p>Check the environment variables in your Vercel project settings, then redeploy.</p>",
        msg.replace('<', "&lt;")
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Requests that reach the function by its own path (`/api/index`) are the site root.
fn normalize(mut req: Request<Body>) -> Request<Body> {
    if req.uri().path() == "/api/index" {
        let pq = match req.uri().query() {
            Some(q) => format!("/?{q}"),
            None => "/".to_string(),
        };
        if let Ok(uri) = pq.parse::<Uri>() {
            *req.uri_mut() = uri;
        }
    }
    req
}

async fn handle(req: Request<Body>) -> Response {
    // A failed start (e.g. missing env vars) isn't cached, so fixing the
    // config and redeploying, or a transient DB error, recovers on the next request.
    let app = match APP.get_or_try_init(build_app).await {
        Ok(app) => app.clone(),
        Err(msg) => return setup_error(&msg),
    };
    match app.oneshot(normalize(req)).await {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let service = tower::ServiceBuilder::new()
        .layer(VercelLayer::new())
        .service(tower::service_fn(|req| async move {
            Ok::<_, std::convert::Infallible>(handle(req).await)
        }));
    vercel_runtime::run(service).await
}
