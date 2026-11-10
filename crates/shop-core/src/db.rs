//! The shop database.
//!
//! Runs on either a local SQLite file (the .exe apps) or a hosted Turso/libSQL
//! database (Vercel). All queries are plain SQLite SQL and work on both.
//! Related rows are deleted explicitly instead of relying on foreign-key
//! cascades, because HTTP connections to Turso don't guarantee they're enforced.

use crate::sql::{Driver, QueryResult, Row, SqlError, Stmt, TursoDriver};
use crate::stmt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS images (
        id          INTEGER PRIMARY KEY,
        mime        TEXT    NOT NULL,
        data        BLOB    NOT NULL,
        created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
    )",
    "CREATE TABLE IF NOT EXISTS products (
        id            INTEGER PRIMARY KEY,
        name          TEXT    NOT NULL,
        category      TEXT    NOT NULL DEFAULT '',
        description   TEXT    NOT NULL DEFAULT '',
        price_cents   INTEGER NOT NULL CHECK (price_cents >= 0),
        features      TEXT    NOT NULL DEFAULT '[]',
        available_for TEXT    NOT NULL DEFAULT '[]',
        image_id      INTEGER,
        created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
        updated_at    TEXT    NOT NULL DEFAULT (datetime('now'))
    )",
    "CREATE TABLE IF NOT EXISTS variations (
        id          INTEGER PRIMARY KEY,
        product_id  INTEGER NOT NULL,
        name        TEXT    NOT NULL,
        price_cents INTEGER CHECK (price_cents IS NULL OR price_cents >= 0),
        position    INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE INDEX IF NOT EXISTS idx_variations_product ON variations(product_id)",
    "CREATE TABLE IF NOT EXISTS featured (
        product_id  INTEGER PRIMARY KEY,
        position    INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS users (
        id            INTEGER PRIMARY KEY,
        name          TEXT NOT NULL,
        email         TEXT NOT NULL UNIQUE COLLATE NOCASE,
        password_hash TEXT NOT NULL,
        created_at    TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    "CREATE TABLE IF NOT EXISTS sessions (
        token       TEXT    PRIMARY KEY,
        user_id     INTEGER NOT NULL,
        expires_at  INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS password_resets (
        user_id     INTEGER PRIMARY KEY,
        code_hash   TEXT    NOT NULL,
        expires_at  INTEGER NOT NULL,
        attempts    INTEGER NOT NULL DEFAULT 0,
        sent_at     INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS admin_sessions (
        token       TEXT    PRIMARY KEY,
        expires_at  INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS cart_items (
        id            INTEGER PRIMARY KEY,
        user_id       INTEGER NOT NULL,
        product_id    INTEGER NOT NULL,
        variation_id  INTEGER NOT NULL DEFAULT 0,
        quantity      INTEGER NOT NULL CHECK (quantity > 0),
        added_at      TEXT    NOT NULL DEFAULT (datetime('now')),
        UNIQUE (user_id, product_id, variation_id)
    )",
    "CREATE INDEX IF NOT EXISTS idx_cart_user ON cart_items(user_id)",
    "CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )",
];

pub const MAX_CART_QTY: i64 = 999;
const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 30;
const ADMIN_SESSION_TTL_SECS: i64 = 60 * 60 * 12;
/// How long a password-reset code stays valid.
pub const RESET_CODE_TTL_SECS: i64 = 10 * 60;
/// Wrong guesses allowed per code before it is thrown away.
pub const RESET_MAX_ATTEMPTS: i64 = 5;
/// Minimum gap between two codes for the same account.
pub const RESET_COOLDOWN_SECS: i64 = 60;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum DbError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Sql(SqlError),
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::NotFound => write!(f, "Not found"),
            DbError::Invalid(m) | DbError::Conflict(m) => write!(f, "{m}"),
            DbError::Sql(e) => write!(f, "Database error: {e}"),
        }
    }
}

impl std::error::Error for DbError {}

impl From<SqlError> for DbError {
    fn from(e: SqlError) -> Self {
        DbError::Sql(e)
    }
}

pub type DbResult<T> = Result<T, DbError>;

fn invalid<T>(msg: &str) -> DbResult<T> {
    Err(DbError::Invalid(msg.to_string()))
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Variation {
    #[serde(default)]
    pub id: Option<i64>,
    pub name: String,
    /// Overrides the product price when set.
    #[serde(default)]
    pub price_cents: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Product {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub description: String,
    pub price_cents: i64,
    pub features: Vec<String>,
    pub available_for: Vec<String>,
    pub image_id: Option<i64>,
    pub variations: Vec<Variation>,
    pub featured: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProductInput {
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    pub price_cents: i64,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub available_for: Vec<String>,
    #[serde(default)]
    pub image_id: Option<i64>,
    #[serde(default)]
    pub variations: Vec<Variation>,
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
        .collect()
}

impl ProductInput {
    fn clean(mut self) -> DbResult<Self> {
        self.name = self.name.trim().to_string();
        self.category = self.category.trim().to_string();
        self.description = self.description.trim().to_string();
        if self.name.is_empty() {
            return invalid("Product name is required");
        }
        if self.name.chars().count() > 200 {
            return invalid("Product name is too long (200 characters max)");
        }
        if self.price_cents < 0 {
            return invalid("Price can't be negative");
        }
        self.features = clean_list(self.features);
        self.available_for = clean_list(self.available_for);
        for v in &mut self.variations {
            v.name = v.name.trim().to_string();
            if v.name.is_empty() {
                return invalid("Every variation needs a name");
            }
            if matches!(v.price_cents, Some(p) if p < 0) {
                return invalid("Variation prices can't be negative");
            }
        }
        let mut names = HashSet::new();
        if !self
            .variations
            .iter()
            .all(|v| names.insert(v.name.to_lowercase()))
        {
            return invalid("Variation names must be unique");
        }
        Ok(self)
    }

    fn features_json(&self) -> String {
        serde_json::to_string(&self.features).unwrap_or_else(|_| "[]".into())
    }

    fn available_json(&self) -> String {
        serde_json::to_string(&self.available_for).unwrap_or_else(|_| "[]".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub shop_name: String,
    pub tagline: String,
    pub currency: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shop_name: "JCS MotorParts".into(),
            tagline: "Genuine and aftermarket motorcycle parts, ready to ride.".into(),
            currency: "₱".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CustomerSummary {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub created_at: String,
    pub cart_quantity: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CartLine {
    pub id: i64,
    pub product_id: i64,
    pub product_name: String,
    pub image_id: Option<i64>,
    pub variation_id: Option<i64>,
    pub variation_name: Option<String>,
    pub unit_price_cents: i64,
    pub quantity: i64,
    pub line_total_cents: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cart {
    pub items: Vec<CartLine>,
    pub item_count: i64,
    pub total_cents: i64,
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

const PRODUCT_COLS: &str = "p.id, p.name, p.category, p.description, p.price_cents, \
    p.features, p.available_for, p.image_id, p.created_at, \
    EXISTS (SELECT 1 FROM featured f WHERE f.product_id = p.id)";

fn map_product(r: &Row) -> Product {
    Product {
        id: r.int(0),
        name: r.text(1),
        category: r.text(2),
        description: r.text(3),
        price_cents: r.int(4),
        features: serde_json::from_str(&r.text(5)).unwrap_or_default(),
        available_for: serde_json::from_str(&r.text(6)).unwrap_or_default(),
        image_id: r.opt_int(7),
        created_at: r.text(8),
        featured: r.int(9) != 0,
        variations: Vec::new(),
    }
}

fn map_user(r: &Row) -> User {
    User {
        id: r.int(0),
        name: r.text(1),
        email: r.text(2),
        created_at: r.text(3),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where `shop.db` lives for the .exe apps: `$SHOP_DB` if set, otherwise next
/// to the executable, so both .exe files in the same folder share one database.
pub fn default_db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SHOP_DB") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("shop.db")))
        .unwrap_or_else(|| PathBuf::from("shop.db"))
}

// ---------------------------------------------------------------------------
// Database handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Db {
    driver: Driver,
    location: String,
}

impl Db {
    /// Open (and create if needed) a local SQLite file.
    #[cfg(feature = "local")]
    pub async fn open_local(path: &std::path::Path) -> DbResult<Self> {
        let driver = Driver::Local(crate::sql::local::LocalDriver::open(path)?);
        let db = Db {
            driver,
            location: path.display().to_string(),
        };
        db.init().await?;
        Ok(db)
    }

    /// Connect to a Turso/libSQL database over HTTP.
    pub async fn open_turso(url: &str, token: Option<String>) -> DbResult<Self> {
        let driver = Driver::Turso(TursoDriver::new(url, token)?);
        let db = Db {
            driver,
            location: url.trim().to_string(),
        };
        db.init().await?;
        Ok(db)
    }

    /// `TURSO_DATABASE_URL` (+ `TURSO_AUTH_TOKEN`) if set, otherwise the local
    /// `shop.db` file (only when built with the `local` feature).
    pub async fn from_env() -> DbResult<Self> {
        let url = std::env::var("TURSO_DATABASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty());
        if let Some(url) = url {
            let token = std::env::var("TURSO_AUTH_TOKEN").ok();
            return Self::open_turso(&url, token).await;
        }
        #[cfg(feature = "local")]
        {
            Self::open_local(&default_db_path()).await
        }
        #[cfg(not(feature = "local"))]
        {
            Err(DbError::Invalid(
                "TURSO_DATABASE_URL is not set. Add it (and TURSO_AUTH_TOKEN) in your Vercel project's Environment Variables."
                    .into(),
            ))
        }
    }

    async fn init(&self) -> DbResult<()> {
        let stmts = SCHEMA.iter().map(|s| Stmt::new(*s, Vec::new())).collect();
        self.driver.batch(stmts).await?;
        Ok(())
    }

    /// Human-readable location (file path or database URL without credentials).
    pub fn location(&self) -> &str {
        &self.location
    }

    pub fn is_local(&self) -> bool {
        self.driver.is_local()
    }

    async fn query(&self, s: Stmt) -> DbResult<QueryResult> {
        Ok(self.driver.query(s).await?)
    }

    async fn rows(&self, s: Stmt) -> DbResult<Vec<Row>> {
        Ok(self.query(s).await?.rows)
    }

    async fn one(&self, s: Stmt) -> DbResult<Option<Row>> {
        Ok(self.query(s).await?.rows.into_iter().next())
    }

    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<QueryResult>> {
        Ok(self.driver.batch(stmts).await?)
    }

    // ----- images ---------------------------------------------------------

    pub async fn insert_image(&self, mime: &str, data: &[u8]) -> DbResult<i64> {
        let row = self
            .one(stmt!(
                "INSERT INTO images (mime, data) VALUES (?1, ?2) RETURNING id",
                mime,
                data.to_vec()
            ))
            .await?
            .ok_or(DbError::NotFound)?;
        Ok(row.int(0))
    }

    pub async fn get_image(&self, id: i64) -> DbResult<(String, Vec<u8>)> {
        let r = self
            .one(stmt!("SELECT mime, data FROM images WHERE id = ?1", id))
            .await?
            .ok_or(DbError::NotFound)?;
        Ok((r.text(0), r.blob(1)))
    }

    async fn image_exists(&self, id: i64) -> DbResult<bool> {
        Ok(self
            .one(stmt!("SELECT 1 FROM images WHERE id = ?1", id))
            .await?
            .is_some())
    }

    // ----- products -------------------------------------------------------

    /// Attach variations to products with one query.
    async fn with_variations(&self, mut products: Vec<Product>) -> DbResult<Vec<Product>> {
        if products.is_empty() {
            return Ok(products);
        }
        let ids: Vec<String> = products.iter().map(|p| p.id.to_string()).collect();
        // ids are integers we produced, so inlining them is safe.
        let sql = format!(
            "SELECT product_id, id, name, price_cents FROM variations
             WHERE product_id IN ({}) ORDER BY position, id",
            ids.join(",")
        );
        for r in self.rows(Stmt::new(sql, Vec::new())).await? {
            let pid = r.int(0);
            if let Some(p) = products.iter_mut().find(|p| p.id == pid) {
                p.variations.push(Variation {
                    id: Some(r.int(1)),
                    name: r.text(2),
                    price_cents: r.opt_int(3),
                });
            }
        }
        Ok(products)
    }

    /// `query` matches name, description, category and "available for";
    /// `category` filters exactly. Empty strings mean "no filter".
    pub async fn list_products(&self, query: &str, category: &str) -> DbResult<Vec<Product>> {
        let q = query.trim().to_string();
        let like = format!("%{q}%");
        let sql = format!(
            "SELECT {PRODUCT_COLS} FROM products p
             WHERE (?1 = '' OR p.name LIKE ?2 OR p.description LIKE ?2
                    OR p.category LIKE ?2 OR p.available_for LIKE ?2)
               AND (?3 = '' OR p.category = ?3)
             ORDER BY p.id DESC"
        );
        let products = self
            .rows(Stmt::new(
                sql,
                vec![q.into(), like.into(), category.trim().into()],
            ))
            .await?
            .iter()
            .map(map_product)
            .collect();
        self.with_variations(products).await
    }

    pub async fn categories(&self) -> DbResult<Vec<String>> {
        Ok(self
            .rows(stmt!(
                "SELECT DISTINCT category FROM products WHERE category <> ''
                 ORDER BY category COLLATE NOCASE"
            ))
            .await?
            .iter()
            .map(|r| r.text(0))
            .collect())
    }

    pub async fn get_product(&self, id: i64) -> DbResult<Product> {
        let sql = format!("SELECT {PRODUCT_COLS} FROM products p WHERE p.id = ?1");
        let p = self
            .one(Stmt::new(sql, vec![id.into()]))
            .await?
            .map(|r| map_product(&r))
            .ok_or(DbError::NotFound)?;
        Ok(self.with_variations(vec![p]).await?.remove(0))
    }

    pub async fn create_product(&self, input: ProductInput) -> DbResult<Product> {
        let input = input.clean()?;
        if let Some(img) = input.image_id {
            if !self.image_exists(img).await? {
                return invalid("Uploaded image was not found, please upload it again");
            }
        }
        let mut stmts = vec![stmt!(
            "INSERT INTO products (name, category, description, price_cents, features, available_for, image_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
            input.name.as_str(),
            input.category.as_str(),
            input.description.as_str(),
            input.price_cents,
            input.features_json(),
            input.available_json(),
            input.image_id
        )];
        // Inside the transaction the newest product row is ours.
        for (pos, v) in input.variations.iter().enumerate() {
            stmts.push(stmt!(
                "INSERT INTO variations (product_id, name, price_cents, position)
                 VALUES ((SELECT MAX(id) FROM products), ?1, ?2, ?3)",
                v.name.as_str(),
                v.price_cents,
                pos as i64
            ));
        }
        let results = self.batch(stmts).await?;
        let id = results
            .first()
            .and_then(|r| r.first())
            .map(|r| r.int(0))
            .ok_or(DbError::NotFound)?;
        self.get_product(id).await
    }

    pub async fn update_product(&self, id: i64, input: ProductInput) -> DbResult<Product> {
        let input = input.clean()?;
        let old_image = self
            .one(stmt!("SELECT image_id FROM products WHERE id = ?1", id))
            .await?
            .ok_or(DbError::NotFound)?
            .opt_int(0);
        if let Some(img) = input.image_id {
            if Some(img) != old_image && !self.image_exists(img).await? {
                return invalid("Uploaded image was not found, please upload it again");
            }
        }
        let existing: HashSet<i64> = self
            .rows(stmt!("SELECT id FROM variations WHERE product_id = ?1", id))
            .await?
            .iter()
            .map(|r| r.int(0))
            .collect();
        let kept: HashSet<i64> = input
            .variations
            .iter()
            .filter_map(|v| v.id)
            .filter(|vid| existing.contains(vid))
            .collect();

        let mut stmts = vec![stmt!(
            "UPDATE products SET name = ?1, category = ?2, description = ?3, price_cents = ?4,
                 features = ?5, available_for = ?6, image_id = ?7, updated_at = datetime('now')
             WHERE id = ?8",
            input.name.as_str(),
            input.category.as_str(),
            input.description.as_str(),
            input.price_cents,
            input.features_json(),
            input.available_json(),
            input.image_id,
            id
        )];
        // Sync variations in place so existing cart lines keep pointing at them.
        for removed in existing.difference(&kept) {
            stmts.push(stmt!(
                "DELETE FROM cart_items WHERE product_id = ?1 AND variation_id = ?2",
                id,
                *removed
            ));
            stmts.push(stmt!("DELETE FROM variations WHERE id = ?1", *removed));
        }
        for (pos, v) in input.variations.iter().enumerate() {
            match v.id.filter(|vid| kept.contains(vid)) {
                Some(vid) => stmts.push(stmt!(
                    "UPDATE variations SET name = ?1, price_cents = ?2, position = ?3 WHERE id = ?4",
                    v.name.as_str(),
                    v.price_cents,
                    pos as i64,
                    vid
                )),
                None => stmts.push(stmt!(
                    "INSERT INTO variations (product_id, name, price_cents, position) VALUES (?1, ?2, ?3, ?4)",
                    id,
                    v.name.as_str(),
                    v.price_cents,
                    pos as i64
                )),
            }
        }
        // A product that gains variations invalidates cart lines without one.
        if existing.is_empty() && !input.variations.is_empty() {
            stmts.push(stmt!(
                "DELETE FROM cart_items WHERE product_id = ?1 AND variation_id = 0",
                id
            ));
        }
        if let Some(old) = old_image.filter(|o| Some(*o) != input.image_id) {
            stmts.push(stmt!("DELETE FROM images WHERE id = ?1", old));
        }
        self.batch(stmts).await?;
        self.get_product(id).await
    }

    pub async fn delete_product(&self, id: i64) -> DbResult<()> {
        let image = self
            .one(stmt!("SELECT image_id FROM products WHERE id = ?1", id))
            .await?
            .ok_or(DbError::NotFound)?
            .opt_int(0);
        let mut stmts = vec![
            stmt!("DELETE FROM cart_items WHERE product_id = ?1", id),
            stmt!("DELETE FROM featured WHERE product_id = ?1", id),
            stmt!("DELETE FROM variations WHERE product_id = ?1", id),
            stmt!("DELETE FROM products WHERE id = ?1", id),
        ];
        if let Some(img) = image {
            stmts.push(stmt!("DELETE FROM images WHERE id = ?1", img));
        }
        self.batch(stmts).await?;
        Ok(())
    }

    // ----- featured -------------------------------------------------------

    pub async fn list_featured(&self) -> DbResult<Vec<Product>> {
        let sql = format!(
            "SELECT {PRODUCT_COLS} FROM featured f JOIN products p ON p.id = f.product_id
             ORDER BY f.position, p.id"
        );
        let products = self
            .rows(Stmt::new(sql, Vec::new()))
            .await?
            .iter()
            .map(map_product)
            .collect();
        self.with_variations(products).await
    }

    /// Replace the featured list with `ids`, in order. Unknown ids are ignored.
    pub async fn set_featured(&self, ids: &[i64]) -> DbResult<Vec<Product>> {
        let mut stmts = vec![stmt!("DELETE FROM featured")];
        let mut seen = HashSet::new();
        for (pos, &id) in ids.iter().filter(|id| seen.insert(**id)).enumerate() {
            stmts.push(stmt!(
                "INSERT INTO featured (product_id, position) SELECT id, ?2 FROM products WHERE id = ?1",
                id,
                pos as i64
            ));
        }
        self.batch(stmts).await?;
        self.list_featured().await
    }

    // ----- settings -------------------------------------------------------

    pub async fn get_settings(&self) -> DbResult<Settings> {
        let mut s = Settings::default();
        for r in self.rows(stmt!("SELECT key, value FROM settings")).await? {
            let v = r.text(1);
            match r.text(0).as_str() {
                "shop_name" => s.shop_name = v,
                "tagline" => s.tagline = v,
                "currency" => s.currency = v,
                _ => {}
            }
        }
        Ok(s)
    }

    pub async fn save_settings(&self, s: Settings) -> DbResult<Settings> {
        let shop_name = s.shop_name.trim().to_string();
        let currency = s.currency.trim().to_string();
        if shop_name.is_empty() {
            return invalid("Shop name is required");
        }
        if currency.is_empty() || currency.chars().count() > 5 {
            return invalid("Currency symbol must be 1–5 characters");
        }
        let stmts = [
            ("shop_name", shop_name),
            ("tagline", s.tagline.trim().to_string()),
            ("currency", currency),
        ]
        .into_iter()
        .map(|(k, v)| {
            stmt!(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                k,
                v
            )
        })
        .collect();
        self.batch(stmts).await?;
        self.get_settings().await
    }

    // ----- users & sessions -----------------------------------------------

    pub async fn create_user(
        &self,
        name: &str,
        email: &str,
        password_hash: &str,
    ) -> DbResult<User> {
        let res = self
            .one(stmt!(
                "INSERT INTO users (name, email, password_hash) VALUES (?1, ?2, ?3)
                 RETURNING id, name, email, created_at",
                name.trim(),
                email.trim().to_lowercase(),
                password_hash
            ))
            .await;
        match res {
            Ok(Some(r)) => Ok(map_user(&r)),
            Ok(None) => Err(DbError::NotFound),
            Err(DbError::Sql(e)) if e.unique_violation => Err(DbError::Conflict(
                "An account with that email already exists".into(),
            )),
            Err(e) => Err(e),
        }
    }

    /// Returns the user and their password hash.
    pub async fn user_credentials(&self, email: &str) -> DbResult<Option<(User, String)>> {
        Ok(self
            .one(stmt!(
                "SELECT id, name, email, created_at, password_hash FROM users WHERE email = ?1",
                email.trim()
            ))
            .await?
            .map(|r| (map_user(&r), r.text(4))))
    }

    pub async fn create_session(&self, user_id: i64, token: &str) -> DbResult<()> {
        let now = now_unix();
        self.batch(vec![
            stmt!("DELETE FROM sessions WHERE expires_at < ?1", now),
            stmt!(
                "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
                token,
                user_id,
                now + SESSION_TTL_SECS
            ),
        ])
        .await?;
        Ok(())
    }

    pub async fn session_user(&self, token: &str) -> DbResult<Option<User>> {
        Ok(self
            .one(stmt!(
                "SELECT u.id, u.name, u.email, u.created_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token = ?1 AND s.expires_at > ?2",
                token,
                now_unix()
            ))
            .await?
            .map(|r| map_user(&r)))
    }

    pub async fn delete_session(&self, token: &str) -> DbResult<()> {
        self.query(stmt!("DELETE FROM sessions WHERE token = ?1", token))
            .await?;
        Ok(())
    }

    // ----- password reset ---------------------------------------------------

    /// Seconds still to wait before another code may be sent (0 = go ahead).
    pub async fn reset_cooldown_left(&self, user_id: i64) -> DbResult<i64> {
        let sent = self
            .one(stmt!(
                "SELECT sent_at FROM password_resets WHERE user_id = ?1",
                user_id
            ))
            .await?
            .map(|r| r.int(0))
            .unwrap_or(0);
        Ok((sent + RESET_COOLDOWN_SECS - now_unix()).max(0))
    }

    /// Store (or replace) the hashed reset code for an account.
    pub async fn save_reset_code(&self, user_id: i64, code_hash: &str) -> DbResult<()> {
        let now = now_unix();
        self.batch(vec![
            stmt!("DELETE FROM password_resets WHERE expires_at < ?1", now),
            stmt!(
                "INSERT INTO password_resets (user_id, code_hash, expires_at, attempts, sent_at)
                 VALUES (?1, ?2, ?3, 0, ?4)
                 ON CONFLICT(user_id) DO UPDATE SET
                    code_hash = excluded.code_hash,
                    expires_at = excluded.expires_at,
                    attempts = 0,
                    sent_at = excluded.sent_at",
                user_id,
                code_hash,
                now + RESET_CODE_TTL_SECS,
                now
            ),
        ])
        .await?;
        Ok(())
    }

    /// Count one guess against the account's code and return the code hash to
    /// check it against. `None` when there is no usable code (none sent, expired,
    /// or too many wrong guesses). Counting first means parallel guesses can't
    /// sneak past the limit.
    pub async fn take_reset_attempt(&self, user_id: i64) -> DbResult<Option<String>> {
        let row = self
            .one(stmt!(
                "UPDATE password_resets SET attempts = attempts + 1
                 WHERE user_id = ?1
                 RETURNING code_hash, expires_at, attempts",
                user_id
            ))
            .await?;
        let Some(r) = row else {
            return Ok(None);
        };
        if r.int(1) <= now_unix() || r.int(2) > RESET_MAX_ATTEMPTS {
            self.delete_reset_code(user_id).await?;
            return Ok(None);
        }
        Ok(Some(r.text(0)))
    }

    pub async fn delete_reset_code(&self, user_id: i64) -> DbResult<()> {
        self.query(stmt!(
            "DELETE FROM password_resets WHERE user_id = ?1",
            user_id
        ))
        .await?;
        Ok(())
    }

    /// Set a new password hash and sign the account out everywhere.
    pub async fn set_password(&self, user_id: i64, password_hash: &str) -> DbResult<()> {
        self.batch(vec![
            stmt!(
                "UPDATE users SET password_hash = ?1 WHERE id = ?2",
                password_hash,
                user_id
            ),
            stmt!("DELETE FROM sessions WHERE user_id = ?1", user_id),
            stmt!("DELETE FROM password_resets WHERE user_id = ?1", user_id),
        ])
        .await?;
        Ok(())
    }

    // ----- admin sessions ---------------------------------------------------

    pub async fn create_admin_session(&self, token: &str) -> DbResult<()> {
        let now = now_unix();
        self.batch(vec![
            stmt!("DELETE FROM admin_sessions WHERE expires_at < ?1", now),
            stmt!(
                "INSERT INTO admin_sessions (token, expires_at) VALUES (?1, ?2)",
                token,
                now + ADMIN_SESSION_TTL_SECS
            ),
        ])
        .await?;
        Ok(())
    }

    pub async fn admin_session_valid(&self, token: &str) -> DbResult<bool> {
        Ok(self
            .one(stmt!(
                "SELECT 1 FROM admin_sessions WHERE token = ?1 AND expires_at > ?2",
                token,
                now_unix()
            ))
            .await?
            .is_some())
    }

    pub async fn delete_admin_session(&self, token: &str) -> DbResult<()> {
        self.query(stmt!("DELETE FROM admin_sessions WHERE token = ?1", token))
            .await?;
        Ok(())
    }

    pub async fn list_customers(&self) -> DbResult<Vec<CustomerSummary>> {
        Ok(self
            .rows(stmt!(
                "SELECT u.id, u.name, u.email, u.created_at,
                        COALESCE((SELECT SUM(c.quantity) FROM cart_items c WHERE c.user_id = u.id), 0)
                 FROM users u ORDER BY u.id DESC"
            ))
            .await?
            .iter()
            .map(|r| CustomerSummary {
                id: r.int(0),
                name: r.text(1),
                email: r.text(2),
                created_at: r.text(3),
                cart_quantity: r.int(4),
            })
            .collect())
    }

    /// Permanently remove a customer account with everything tied to it:
    /// sign-in sessions, pending reset codes and cart items.
    pub async fn delete_customer(&self, id: i64) -> DbResult<()> {
        let exists = self
            .one(stmt!("SELECT 1 FROM users WHERE id = ?1", id))
            .await?
            .is_some();
        if !exists {
            return Err(DbError::NotFound);
        }
        self.batch(vec![
            stmt!("DELETE FROM sessions WHERE user_id = ?1", id),
            stmt!("DELETE FROM password_resets WHERE user_id = ?1", id),
            stmt!("DELETE FROM cart_items WHERE user_id = ?1", id),
            stmt!("DELETE FROM users WHERE id = ?1", id),
        ])
        .await?;
        Ok(())
    }

    // ----- cart -----------------------------------------------------------

    pub async fn cart(&self, user_id: i64) -> DbResult<Cart> {
        let items: Vec<CartLine> = self
            .rows(stmt!(
                "SELECT c.id, c.product_id, p.name, p.image_id, v.id, v.name,
                        COALESCE(v.price_cents, p.price_cents), c.quantity
                 FROM cart_items c
                 JOIN products p ON p.id = c.product_id
                 LEFT JOIN variations v ON v.id = c.variation_id AND c.variation_id <> 0
                 WHERE c.user_id = ?1
                 ORDER BY c.id",
                user_id
            ))
            .await?
            .iter()
            .map(|r| {
                let unit = r.int(6);
                let qty = r.int(7);
                CartLine {
                    id: r.int(0),
                    product_id: r.int(1),
                    product_name: r.text(2),
                    image_id: r.opt_int(3),
                    variation_id: r.opt_int(4),
                    variation_name: r.opt_text(5),
                    unit_price_cents: unit,
                    quantity: qty,
                    line_total_cents: unit * qty,
                }
            })
            .collect();
        Ok(Cart {
            item_count: items.iter().map(|i| i.quantity).sum(),
            total_cents: items.iter().map(|i| i.line_total_cents).sum(),
            items,
        })
    }

    pub async fn add_to_cart(
        &self,
        user_id: i64,
        product_id: i64,
        variation_id: Option<i64>,
        quantity: i64,
    ) -> DbResult<()> {
        if !(1..=MAX_CART_QTY).contains(&quantity) {
            return invalid("Quantity must be between 1 and 999");
        }
        let r = self
            .one(stmt!(
                "SELECT (SELECT COUNT(*) FROM products WHERE id = ?1),
                        (SELECT COUNT(*) FROM variations WHERE product_id = ?1),
                        (SELECT COUNT(*) FROM variations WHERE product_id = ?1 AND id = ?2)",
                product_id,
                variation_id.unwrap_or(0)
            ))
            .await?
            .ok_or(DbError::NotFound)?;
        if r.int(0) == 0 {
            return Err(DbError::NotFound);
        }
        let vid = match (r.int(1) > 0, variation_id.filter(|v| *v != 0)) {
            (true, Some(v)) if r.int(2) > 0 => v,
            (true, Some(_)) => return invalid("That variation is no longer available"),
            (true, None) => return invalid("Please choose a variation"),
            (false, _) => 0,
        };
        self.query(stmt!(
            "INSERT INTO cart_items (user_id, product_id, variation_id, quantity)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (user_id, product_id, variation_id)
             DO UPDATE SET quantity = MIN(quantity + excluded.quantity, ?5)",
            user_id,
            product_id,
            vid,
            quantity,
            MAX_CART_QTY
        ))
        .await?;
        Ok(())
    }

    /// Set a cart line's quantity; 0 or less removes the line.
    pub async fn set_cart_quantity(
        &self,
        user_id: i64,
        item_id: i64,
        quantity: i64,
    ) -> DbResult<()> {
        let res = if quantity <= 0 {
            self.query(stmt!(
                "DELETE FROM cart_items WHERE id = ?1 AND user_id = ?2",
                item_id,
                user_id
            ))
            .await?
        } else {
            self.query(stmt!(
                "UPDATE cart_items SET quantity = ?1 WHERE id = ?2 AND user_id = ?3",
                quantity.min(MAX_CART_QTY),
                item_id,
                user_id
            ))
            .await?
        };
        if res.affected == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    pub async fn clear_cart(&self, user_id: i64) -> DbResult<()> {
        self.query(stmt!("DELETE FROM cart_items WHERE user_id = ?1", user_id))
            .await?;
        Ok(())
    }

    // ----- demo data ------------------------------------------------------

    /// Adds a handful of sample parts. Only allowed while the catalog is empty.
    pub async fn seed_demo(&self) -> DbResult<usize> {
        let count = self
            .one(stmt!("SELECT COUNT(*) FROM products"))
            .await?
            .map(|r| r.int(0))
            .unwrap_or(0);
        if count > 0 {
            return Err(DbError::Conflict(
                "Sample products can only be loaded into an empty catalog".into(),
            ));
        }
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let var = |name: &str, price: Option<i64>| Variation {
            id: None,
            name: name.into(),
            price_cents: price,
        };
        let samples = vec![
            ProductInput {
                name: "4-Piston Front Brake Caliper".into(),
                category: "Brakes".into(),
                description: "CNC-machined radial caliper with stainless pistons for stronger, more consistent stopping power on daily rides and spirited weekends.".into(),
                price_cents: 385_000,
                features: s(&["CNC-machined 6061 aluminum body", "Stainless steel pistons", "Includes pads and mounting bolts", "Anodized finish that resists fading"]),
                available_for: s(&["Yamaha NMAX 155", "Yamaha Aerox 155", "Honda Click 125i / 150i", "Honda ADV 160"]),
                image_id: None,
                variations: vec![var("Black", None), var("Red", None), var("Gold", Some(405_000))],
            },
            ProductInput {
                name: "Iridium Spark Plug".into(),
                category: "Engine".into(),
                description: "Fine-wire iridium tip for quicker starts, smoother idle and longer service life than standard nickel plugs.".into(),
                price_cents: 45_000,
                features: s(&["0.6 mm iridium center electrode", "Up to 3× longer life", "Pre-gapped from the factory"]),
                available_for: s(&["Honda Click 125i", "Honda Beat", "Suzuki Raider R150", "Yamaha Mio i 125"]),
                image_id: None,
                variations: vec![var("CR7E", None), var("CR8E", None), var("CPR8EA-9", Some(52_000))],
            },
            ProductInput {
                name: "O-Ring Drive Chain 428H".into(),
                category: "Drivetrain".into(),
                description: "Heavy-duty sealed chain that keeps grease in and grit out, so it stretches less and lasts longer.".into(),
                price_cents: 120_000,
                features: s(&["Sealed O-rings", "Heat-treated pins and bushings", "Includes master link"]),
                available_for: s(&["Honda XRM 125", "Suzuki Smash 115", "Kawasaki Barako 175", "Yamaha Sniper 155"]),
                image_id: None,
                variations: vec![var("120 links", None), var("130 links", Some(132_000))],
            },
            ProductInput {
                name: "LED Projector Headlight".into(),
                category: "Lighting".into(),
                description: "Plug-and-play LED projector with a sharp cutoff line that lights the road without blinding oncoming traffic.".into(),
                price_cents: 275_000,
                features: s(&["6000K white output", "Built-in cooling fan", "Plug-and-play harness", "IP67 water resistance"]),
                available_for: s(&["Yamaha NMAX 155 (2020+)", "Yamaha Aerox 155"]),
                image_id: None,
                variations: vec![],
            },
            ProductInput {
                name: "Fully Synthetic 10W-40 Engine Oil".into(),
                category: "Oils & Fluids".into(),
                description: "JASO MA2 rated synthetic oil that protects at high RPM and keeps wet clutches engaging cleanly.".into(),
                price_cents: 52_000,
                features: s(&["JASO MA2 / API SN", "Wet-clutch safe", "Stable under high heat"]),
                available_for: s(&["All 4-stroke motorcycles", "Scooters with wet clutch"]),
                image_id: None,
                variations: vec![var("0.8 L", Some(43_000)), var("1 L", None)],
            },
        ];
        let mut ids = Vec::new();
        for p in samples {
            ids.push(self.create_product(p).await?.id);
        }
        let featured: Vec<i64> = ids.iter().copied().take(3).collect();
        self.set_featured(&featured).await?;
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "local")]
    #[tokio::test]
    async fn scenario_local_sqlite() {
        let dir = std::env::temp_dir().join(format!("shop-test-{}", crate::auth::new_token()));
        scenario(Db::open_local(&dir.join("shop.db")).await.unwrap()).await;
    }

    /// Runs against a Turso-compatible server when `TURSO_TEST_URL` is set
    /// (each run needs an empty database).
    #[tokio::test]
    async fn scenario_turso_http() {
        let Ok(url) = std::env::var("TURSO_TEST_URL") else {
            eprintln!("TURSO_TEST_URL not set; skipping");
            return;
        };
        let token = std::env::var("TURSO_TEST_TOKEN").ok();
        scenario(Db::open_turso(&url, token).await.unwrap()).await;
    }

    async fn scenario(db: Db) {
        assert_eq!(db.seed_demo().await.unwrap(), 5);
        let all = db.list_products("", "").await.unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(db.list_featured().await.unwrap().len(), 3);
        assert_eq!(db.list_products("nmax", "").await.unwrap().len(), 2);

        let user = db
            .create_user("Ana", "Ana@Example.com", "hash")
            .await
            .unwrap();
        assert_eq!(user.email, "ana@example.com");
        assert!(matches!(
            db.create_user("Ana", "ana@example.com", "hash").await,
            Err(DbError::Conflict(_))
        ));

        let caliper = all.iter().find(|p| p.name.contains("Caliper")).unwrap();
        assert_eq!(caliper.variations.len(), 3);
        let gold = caliper
            .variations
            .iter()
            .find(|v| v.name == "Gold")
            .unwrap();
        assert!(db.add_to_cart(user.id, caliper.id, None, 1).await.is_err());
        db.add_to_cart(user.id, caliper.id, gold.id, 2)
            .await
            .unwrap();
        db.add_to_cart(user.id, caliper.id, gold.id, 1)
            .await
            .unwrap();
        let cart = db.cart(user.id).await.unwrap();
        assert_eq!(cart.items.len(), 1);
        assert_eq!(cart.item_count, 3);
        assert_eq!(cart.total_cents, 3 * 405_000);

        let line = cart.items[0].id;
        db.set_cart_quantity(user.id, line, 1).await.unwrap();
        assert_eq!(db.cart(user.id).await.unwrap().total_cents, 405_000);
        db.set_cart_quantity(user.id, line, 0).await.unwrap();
        assert!(db.cart(user.id).await.unwrap().items.is_empty());

        db.add_to_cart(user.id, caliper.id, gold.id, 1)
            .await
            .unwrap();
        db.delete_product(caliper.id).await.unwrap();
        assert_eq!(db.list_featured().await.unwrap().len(), 2);
        assert!(db.cart(user.id).await.unwrap().items.is_empty());

        // atomic batch: a statement that fails a CHECK leaves nothing behind
        let before = db.list_products("", "").await.unwrap().len();
        let res = db
            .batch(vec![
                stmt!("INSERT INTO products (name, price_cents) VALUES ('Ghost', 1)"),
                stmt!("INSERT INTO products (name, price_cents) VALUES ('Bad', -1)"),
            ])
            .await;
        assert!(res.is_err());
        assert_eq!(db.list_products("", "").await.unwrap().len(), before);

        assert!(db.create_admin_session("tok").await.is_ok());
        assert!(db.admin_session_valid("tok").await.unwrap());
        db.delete_admin_session("tok").await.unwrap();
        assert!(!db.admin_session_valid("tok").await.unwrap());
    }
}
