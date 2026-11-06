//! Customer storefront: sign up / sign in, browse parts, product popup, cart.
//!
//! `router()` is used by the `shop-customer` .exe and by the Vercel function.

use axum::{
    extract::{FromRef, FromRequestParts, Path, Query, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use shop_core::{
    auth, captcha,
    db::{Cart, Product, Settings, User},
    web::{self, ApiError, ApiResult},
    Db,
};

const PAGE: &str = include_str!("../assets/customer.html");
const COOKIE: &str = "shop_session";
const COOKIE_MAX_AGE: i64 = 60 * 60 * 24 * 30;

#[derive(Clone)]
pub struct CustomerState {
    pub db: Db,
    /// Mark cookies `Secure` (true behind HTTPS, e.g. on Vercel).
    pub secure_cookies: bool,
}

impl FromRef<CustomerState> for Db {
    fn from_ref(s: &CustomerState) -> Db {
        s.db.clone()
    }
}

/// All customer routes, served from the site root.
pub fn router(state: CustomerState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/static/base.css", get(web::base_css))
        .route("/static/logo.svg", get(web::logo_svg))
        .route("/static/jcs-logo.svg", get(web::jcs_logo_svg))
        .route("/images/{id}", get(web::image))
        .route("/api/settings", get(settings))
        .route("/api/captcha", get(captcha_config))
        .route("/api/auth/signup", post(signup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/featured", get(featured))
        .route("/api/categories", get(categories))
        .route("/api/products", get(products))
        .route("/api/products/{id}", get(product))
        .route("/api/cart", get(cart).post(add_to_cart).delete(clear_cart))
        .route(
            "/api/cart/{item_id}",
            patch(set_quantity).delete(remove_item),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Session handling
// ---------------------------------------------------------------------------

/// Extractor: the signed-in customer, or 401.
struct CurrentUser(User);

impl FromRequestParts<CustomerState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &CustomerState,
    ) -> Result<Self, Self::Rejection> {
        let token = web::read_cookie(&parts.headers, COOKIE).ok_or_else(ApiError::unauthorized)?;
        state
            .db
            .session_user(&token)
            .await?
            .map(CurrentUser)
            .ok_or_else(ApiError::unauthorized)
    }
}

async fn start_session(state: &CustomerState, user: User) -> Result<impl IntoResponse, ApiError> {
    let token = auth::new_token();
    state.db.create_session(user.id, &token).await?;
    Ok((
        [(
            header::SET_COOKIE,
            web::cookie(COOKIE, &token, COOKIE_MAX_AGE, state.secure_cookies),
        )],
        Json(user),
    ))
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SignupBody {
    name: String,
    email: String,
    password: String,
    #[serde(default)]
    captcha_token: Option<String>,
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
    #[serde(default)]
    captcha_token: Option<String>,
}

/// `GET /api/captcha` – tells the page whether to show the human check.
async fn captcha_config() -> Json<Value> {
    Json(json!({ "site_key": captcha::site_key() }))
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.contains(char::is_whitespace)
        && email.len() <= 254
}

async fn signup(
    State(state): State<CustomerState>,
    headers: HeaderMap,
    Json(body): Json<SignupBody>,
) -> Result<impl IntoResponse, ApiError> {
    captcha::verify(
        body.captcha_token.as_deref(),
        captcha::client_ip(&headers).as_deref(),
    )
    .await?;
    let name = body.name.trim().to_string();
    let email = body.email.trim().to_lowercase();
    if name.is_empty() {
        return Err(ApiError::bad_request("Please enter your name"));
    }
    if name.chars().count() > 80 {
        return Err(ApiError::bad_request("Name is too long"));
    }
    if !valid_email(&email) {
        return Err(ApiError::bad_request("Please enter a valid email address"));
    }
    if body.password.chars().count() < 6 {
        return Err(ApiError::bad_request(
            "Password must be at least 6 characters",
        ));
    }
    if body.password.len() > 256 {
        return Err(ApiError::bad_request("Password is too long"));
    }
    let password = body.password;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(ApiError::internal)?;
    let user = state.db.create_user(&name, &email, &hash).await?;
    start_session(&state, user).await
}

async fn login(
    State(state): State<CustomerState>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<impl IntoResponse, ApiError> {
    captcha::verify(
        body.captcha_token.as_deref(),
        captcha::client_ip(&headers).as_deref(),
    )
    .await?;
    let wrong = || {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "Incorrect email or password".into(),
        )
    };
    let Some((user, hash)) = state
        .db
        .user_credentials(&body.email.to_lowercase())
        .await?
    else {
        return Err(wrong());
    };
    let password = body.password;
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    if !ok {
        return Err(wrong());
    }
    start_session(&state, user).await
}

async fn logout(
    State(state): State<CustomerState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(token) = web::read_cookie(&headers, COOKIE) {
        state.db.delete_session(&token).await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            web::cookie(COOKIE, "", 0, state.secure_cookies),
        )],
        Json(json!({ "ok": true })),
    ))
}

async fn me(CurrentUser(user): CurrentUser) -> ApiResult<User> {
    Ok(Json(user))
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

async fn settings(State(db): State<Db>) -> ApiResult<Settings> {
    Ok(Json(db.get_settings().await?))
}

async fn featured(State(db): State<Db>, _u: CurrentUser) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_featured().await?))
}

async fn categories(State(db): State<Db>, _u: CurrentUser) -> ApiResult<Vec<String>> {
    Ok(Json(db.categories().await?))
}

#[derive(Deserialize)]
struct ProductQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    category: String,
}

async fn products(
    State(db): State<Db>,
    _u: CurrentUser,
    Query(q): Query<ProductQuery>,
) -> ApiResult<Vec<Product>> {
    Ok(Json(db.list_products(&q.q, &q.category).await?))
}

async fn product(State(db): State<Db>, _u: CurrentUser, Path(id): Path<i64>) -> ApiResult<Product> {
    Ok(Json(db.get_product(id).await?))
}

// ---------------------------------------------------------------------------
// Cart
// ---------------------------------------------------------------------------

async fn cart(State(db): State<Db>, CurrentUser(user): CurrentUser) -> ApiResult<Cart> {
    Ok(Json(db.cart(user.id).await?))
}

#[derive(Deserialize)]
struct AddBody {
    product_id: i64,
    #[serde(default)]
    variation_id: Option<i64>,
    quantity: i64,
}

async fn add_to_cart(
    State(db): State<Db>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<AddBody>,
) -> ApiResult<Cart> {
    db.add_to_cart(user.id, body.product_id, body.variation_id, body.quantity)
        .await?;
    Ok(Json(db.cart(user.id).await?))
}

#[derive(Deserialize)]
struct QtyBody {
    quantity: i64,
}

async fn set_quantity(
    State(db): State<Db>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
    Json(body): Json<QtyBody>,
) -> ApiResult<Cart> {
    db.set_cart_quantity(user.id, item_id, body.quantity)
        .await?;
    Ok(Json(db.cart(user.id).await?))
}

async fn remove_item(
    State(db): State<Db>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
) -> ApiResult<Cart> {
    db.set_cart_quantity(user.id, item_id, 0).await?;
    Ok(Json(db.cart(user.id).await?))
}

async fn clear_cart(State(db): State<Db>, CurrentUser(user): CurrentUser) -> ApiResult<Value> {
    db.clear_cart(user.id).await?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_validation() {
        assert!(valid_email("ana@example.com"));
        assert!(!valid_email("ana@example"));
        assert!(!valid_email("@example.com"));
        assert!(!valid_email("ana example@x.com"));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "a=1; shop_session=abc; b=2".parse().unwrap(),
        );
        assert_eq!(web::read_cookie(&h, COOKIE).as_deref(), Some("abc"));
    }
}
