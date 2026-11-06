//! Admin app: manage the catalog, the featured home screen, store settings,
//! and see registered customers.
//!
//! `router()` is used by the `shop-admin` .exe (served at `/`) and by the
//! Vercel function (mounted at `/admin`).
//!
//! Access control:
//! * `password` set      → everyone must sign in with it (any build).
//! * no password, local  → only requests addressed to this computer are allowed.
//! * no password, online → the admin API refuses to work until one is configured.

use axum::{
    extract::{DefaultBodyLimit, FromRef, Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use shop_core::{
    auth, captcha,
    db::{CustomerSummary, Product, ProductInput, Settings},
    web::{self, ApiError, ApiResult},
    Db,
};
use std::sync::Arc;

const PAGE: &str = include_str!("../assets/admin.html");
const COOKIE: &str = "shop_admin";
const COOKIE_MAX_AGE: i64 = 60 * 60 * 12;
/// Keeps uploads (as base64 JSON) under Vercel's 4.5 MB request limit.
/// The admin page resizes photos before upload, so real uploads are far smaller.
pub const MAX_IMAGE_BYTES: usize = 3 * 1024 * 1024;

#[derive(Clone)]
pub struct AdminConfig {
    /// URL prefix the admin is mounted at: "" for the .exe, "/admin" on Vercel.
    pub base: String,
    /// Admin password (from `ADMIN_PASSWORD`). `None` = no login.
    pub password: Option<String>,
    /// Without a password, allow only requests addressed to this computer.
    pub local_only: bool,
    /// Mark cookies `Secure` (behind HTTPS).
    pub secure_cookies: bool,
    /// Where the "Open store" link points.
    pub store_url: String,
}

#[derive(Clone)]
pub struct AdminState {
    pub db: Db,
    pub cfg: Arc<AdminConfig>,
}

impl FromRef<AdminState> for Db {
    fn from_ref(s: &AdminState) -> Db {
        s.db.clone()
    }
}

pub fn router(db: Db, cfg: AdminConfig) -> Router {
    let page = Arc::new(PAGE.replace("__BASE__", &cfg.base));
    let state = AdminState {
        db,
        cfg: Arc::new(cfg),
    };

    let api = Router::new()
        .route(
            "/api/images",
            post(upload_image).layer(DefaultBodyLimit::max(MAX_IMAGE_BYTES * 2)),
        )
        .route("/api/info", get(info))
        .route("/api/settings", get(get_settings).put(save_settings))
        .route("/api/categories", get(categories))
        .route("/api/products", get(list_products).post(create_product))
        .route(
            "/api/products/{id}",
            get(get_product).put(update_product).delete(delete_product),
        )
        .route("/api/featured", get(get_featured).put(set_featured))
        .route("/api/customers", get(customers))
        .route("/api/demo", post(seed_demo))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));

    let mut app = Router::new()
        .route(
            "/",
            get(move || {
                let page = page.clone();
                async move { Html(page.as_str().to_owned()) }
            }),
        )
        .route("/static/base.css", get(web::base_css))
        .route("/static/logo.svg", get(web::logo_svg))
        .route("/static/jcs-logo.svg", get(web::jcs_logo_svg))
        .route("/images/{id}", get(web::image))
        .route("/api/auth/status", get(auth_status))
        .route("/api/captcha", get(captcha_config))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .merge(api)
        .with_state(state.clone());

    if state.cfg.local_only && state.cfg.password.is_none() {
        app = app.layer(middleware::from_fn(local_only));
    }
    app
}

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

/// Reject requests whose Host isn't this machine (blocks DNS-rebinding tricks).
async fn local_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let name = if host.starts_with('[') {
        host.split(']')
            .next()
            .map(|h| format!("{h}]"))
            .unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    if matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "The admin app only accepts connections from this computer. Set ADMIN_PASSWORD to allow signing in from other devices.",
        )
            .into_response()
    }
}

fn not_configured() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "The admin is locked because ADMIN_PASSWORD isn't set. Add it in your Vercel project's Environment Variables, then redeploy.".into(),
    )
}

async fn is_signed_in(state: &AdminState, headers: &HeaderMap) -> Result<bool, ApiError> {
    match web::read_cookie(headers, COOKIE) {
        Some(token) => Ok(state.db.admin_session_valid(&token).await?),
        None => Ok(false),
    }
}

async fn require_admin(
    State(state): State<AdminState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if state.cfg.password.is_none() {
        if state.cfg.local_only {
            return Ok(next.run(req).await); // guarded by `local_only`
        }
        return Err(not_configured());
    }
    if is_signed_in(&state, req.headers()).await? {
        Ok(next.run(req).await)
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Please sign in to the admin".into(),
        ))
    }
}

async fn auth_status(State(state): State<AdminState>, headers: HeaderMap) -> ApiResult<Value> {
    let required = state.cfg.password.is_some();
    let configured = required || state.cfg.local_only;
    let signed_in = if required {
        is_signed_in(&state, &headers).await?
    } else {
        configured
    };
    Ok(Json(json!({
        "required": required,
        "configured": configured,
        "signed_in": signed_in,
    })))
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
    #[serde(default)]
    captcha_token: Option<String>,
}

/// `GET /api/captcha` – tells the page whether to show the human check.
async fn captcha_config() -> Json<Value> {
    Json(json!({ "site_key": captcha::site_key() }))
}

async fn login(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(expected) = state.cfg.password.as_deref() else {
        return Err(if state.cfg.local_only {
            ApiError::bad_request("This admin doesn't use a password")
        } else {
            not_configured()
        });
    };
    captcha::verify(
        body.captcha_token.as_deref(),
        captcha::client_ip(&headers).as_deref(),
    )
    .await?;
    if !auth::constant_time_eq(body.password.as_str(), expected) {
        // Slow down guessing.
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Wrong password".into()));
    }
    let token = auth::new_token();
    state.db.create_admin_session(&token).await?;
    Ok((
        [(
            header::SET_COOKIE,
            web::cookie(COOKIE, &token, COOKIE_MAX_AGE, state.cfg.secure_cookies),
        )],
        Json(json!({ "ok": true })),
    ))
}

async fn logout(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(token) = web::read_cookie(&headers, COOKIE) {
        state.db.delete_admin_session(&token).await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            web::cookie(COOKIE, "", 0, state.cfg.secure_cookies),
        )],
        Json(json!({ "ok": true })),
    ))
}

async fn info(State(state): State<AdminState>) -> Json<Value> {
    Json(json!({
        "db_path": state.db.location(),
        "db_kind": if state.db.is_local() { "Local file" } else { "Turso" },
        "store_url": state.cfg.store_url,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImageUpload {
    /// `data:image/png;base64,....`
    data_url: String,
}

async fn upload_image(State(db): State<Db>, Json(body): Json<ImageUpload>) -> ApiResult<Value> {
    let bad = || ApiError::bad_request("That file doesn't look like a supported image");
    let rest = body.data_url.strip_prefix("data:").ok_or_else(bad)?;
    let (meta, b64) = rest.split_once(',').ok_or_else(bad)?;
    let mime = meta
        .strip_suffix(";base64")
        .ok_or_else(bad)?
        .to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err(ApiError::bad_request("Use a PNG, JPG, WEBP or GIF image"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| bad())?;
    if bytes.is_empty() {
        return Err(bad());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ApiError::bad_request("Image is too large (3 MB max)"));
    }
    let id = db.insert_image(&mime, &bytes).await?;
    Ok(Json(json!({ "id": id })))
}

// ---------------------------------------------------------------------------
// Products
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProductQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    category: String,
}

async fn list_products(
    State(db): State<Db>,
    Query(q): Query<ProductQuery>,
) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_products(&q.q, &q.category).await?))
}

async fn categories(State(db): State<Db>) -> ApiResult<Vec<String>> {
    Ok(Json(db.categories().await?))
}

async fn get_product(State(db): State<Db>, Path(id): Path<i64>) -> ApiResult<Product> {
    Ok(Json(db.get_product(id).await?))
}

async fn create_product(
    State(db): State<Db>,
    Json(input): Json<ProductInput>,
) -> ApiResult<Product> {
    Ok(Json(db.create_product(input).await?))
}

async fn update_product(
    State(db): State<Db>,
    Path(id): Path<i64>,
    Json(input): Json<ProductInput>,
) -> ApiResult<Product> {
    Ok(Json(db.update_product(id, input).await?))
}

async fn delete_product(State(db): State<Db>, Path(id): Path<i64>) -> ApiResult<Value> {
    db.delete_product(id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Featured / settings / customers
// ---------------------------------------------------------------------------

async fn get_featured(State(db): State<Db>) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_featured().await?))
}

#[derive(Deserialize)]
struct FeaturedBody {
    product_ids: Vec<i64>,
}

async fn set_featured(
    State(db): State<Db>,
    Json(body): Json<FeaturedBody>,
) -> ApiResult<Vec<Product>> {
    Ok(Json(db.set_featured(&body.product_ids).await?))
}

async fn get_settings(State(db): State<Db>) -> ApiResult<Settings> {
    Ok(Json(db.get_settings().await?))
}

async fn save_settings(State(db): State<Db>, Json(s): Json<Settings>) -> ApiResult<Settings> {
    Ok(Json(db.save_settings(s).await?))
}

async fn customers(State(db): State<Db>) -> ApiResult<Vec<CustomerSummary>> {
    Ok(Json(db.list_customers().await?))
}

async fn seed_demo(State(db): State<Db>) -> ApiResult<Value> {
    let n = db.seed_demo().await?;
    Ok(Json(json!({ "added": n })))
}
