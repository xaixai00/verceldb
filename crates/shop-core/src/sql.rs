//! Minimal SQL driver layer with two backends:
//!
//! * **Local** – an SQLite file via `rusqlite` (feature `local`, used by the .exe apps).
//! * **Turso** – a hosted libSQL database over the Hrana-over-HTTP protocol
//!   (`POST /v2/pipeline`), used on Vercel where there is no persistent disk.
//!
//! Both expose the same two operations: run one statement, or run several
//! statements atomically (all succeed or none are applied).

use base64::Engine;
use serde_json::{json, Value as Json};
use std::fmt;

// ---------------------------------------------------------------------------
// Values, rows, statements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Integer(v)
    }
}
impl From<Option<i64>> for Value {
    fn from(v: Option<i64>) -> Self {
        v.map(Value::Integer).unwrap_or(Value::Null)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&String> for Value {
    fn from(v: &String) -> Self {
        Value::Text(v.clone())
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Blob(v)
    }
}

/// A result row. Getters are lenient about NULL/type so schema drift can't panic.
#[derive(Debug, Clone)]
pub struct Row(pub Vec<Value>);

impl Row {
    fn get(&self, i: usize) -> &Value {
        self.0.get(i).unwrap_or(&Value::Null)
    }
    pub fn int(&self, i: usize) -> i64 {
        self.opt_int(i).unwrap_or(0)
    }
    pub fn opt_int(&self, i: usize) -> Option<i64> {
        match self.get(i) {
            Value::Integer(v) => Some(*v),
            Value::Real(v) => Some(*v as i64),
            Value::Text(s) => s.parse().ok(),
            _ => None,
        }
    }
    pub fn text(&self, i: usize) -> String {
        self.opt_text(i).unwrap_or_default()
    }
    pub fn opt_text(&self, i: usize) -> Option<String> {
        match self.get(i) {
            Value::Text(s) => Some(s.clone()),
            Value::Integer(v) => Some(v.to_string()),
            Value::Real(v) => Some(v.to_string()),
            _ => None,
        }
    }
    pub fn blob(&self, i: usize) -> Vec<u8> {
        match self.get(i) {
            Value::Blob(b) => b.clone(),
            Value::Text(s) => s.as_bytes().to_vec(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub sql: String,
    pub args: Vec<Value>,
}

impl Stmt {
    pub fn new(sql: impl Into<String>, args: Vec<Value>) -> Self {
        Stmt {
            sql: sql.into(),
            args,
        }
    }
}

/// Build a statement: `stmt!("SELECT ?1", id)`.
#[macro_export]
macro_rules! stmt {
    ($sql:expr) => { $crate::sql::Stmt::new($sql, Vec::new()) };
    ($sql:expr, $($arg:expr),+ $(,)?) => {
        $crate::sql::Stmt::new($sql, vec![$($crate::sql::Value::from($arg)),+])
    };
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub rows: Vec<Row>,
    pub affected: u64,
}

impl QueryResult {
    pub fn first(&self) -> Option<&Row> {
        self.rows.first()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SqlError {
    pub message: String,
    /// True for UNIQUE/PRIMARY KEY violations, so callers can report "already exists".
    pub unique_violation: bool,
}

impl SqlError {
    fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let unique_violation = message.contains("UNIQUE constraint failed")
            || message.contains("SQLITE_CONSTRAINT_UNIQUE")
            || message.contains("SQLITE_CONSTRAINT_PRIMARYKEY");
        SqlError {
            message,
            unique_violation,
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SqlError {}

pub type SqlResult<T> = Result<T, SqlError>;

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum Driver {
    #[cfg(feature = "local")]
    Local(local::LocalDriver),
    Turso(TursoDriver),
}

impl Driver {
    pub async fn query(&self, stmt: Stmt) -> SqlResult<QueryResult> {
        match self {
            #[cfg(feature = "local")]
            Driver::Local(d) => d.query(stmt).await,
            Driver::Turso(d) => d.query(stmt).await,
        }
    }

    /// Run all statements in one transaction. On any error nothing is applied.
    pub async fn batch(&self, stmts: Vec<Stmt>) -> SqlResult<Vec<QueryResult>> {
        if stmts.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            #[cfg(feature = "local")]
            Driver::Local(d) => d.batch(stmts).await,
            Driver::Turso(d) => d.batch(stmts).await,
        }
    }

    pub fn is_local(&self) -> bool {
        match self {
            #[cfg(feature = "local")]
            Driver::Local(_) => true,
            Driver::Turso(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Local SQLite (rusqlite)
// ---------------------------------------------------------------------------

#[cfg(feature = "local")]
pub mod local {
    use super::{QueryResult, Row, SqlError, SqlResult, Stmt, Value};
    use rusqlite::types::{Value as RValue, ValueRef};
    use rusqlite::Connection;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    pub struct LocalDriver {
        conn: Arc<Mutex<Connection>>,
    }

    fn err(e: rusqlite::Error) -> SqlError {
        let unique = matches!(
            &e,
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation
                    && (f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                        || f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY)
        );
        let mut se = SqlError::new(e.to_string());
        se.unique_violation |= unique;
        se
    }

    fn to_sql(v: &Value) -> RValue {
        match v {
            Value::Null => RValue::Null,
            Value::Integer(i) => RValue::Integer(*i),
            Value::Real(f) => RValue::Real(*f),
            Value::Text(s) => RValue::Text(s.clone()),
            Value::Blob(b) => RValue::Blob(b.clone()),
        }
    }

    fn run(conn: &Connection, stmt: &Stmt) -> rusqlite::Result<QueryResult> {
        let mut st = conn.prepare_cached(&stmt.sql)?;
        let params: Vec<RValue> = stmt.args.iter().map(to_sql).collect();
        let cols = st.column_count();
        let mut rows = Vec::new();
        {
            let mut q = st.query(rusqlite::params_from_iter(params.iter()))?;
            while let Some(r) = q.next()? {
                let mut vals = Vec::with_capacity(cols);
                for i in 0..cols {
                    vals.push(match r.get_ref(i)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(v) => Value::Integer(v),
                        ValueRef::Real(v) => Value::Real(v),
                        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
                        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
                    });
                }
                rows.push(Row(vals));
            }
        }
        Ok(QueryResult {
            rows,
            affected: conn.changes(),
        })
    }

    impl LocalDriver {
        pub fn open(path: &Path) -> SqlResult<Self> {
            if let Some(dir) = path.parent() {
                if !dir.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(dir);
                }
            }
            let conn = Connection::open(path).map_err(err)?;
            conn.busy_timeout(Duration::from_secs(5)).map_err(err)?;
            conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
                .map_err(err)?;
            conn.execute_batch("PRAGMA foreign_keys = ON;")
                .map_err(err)?;
            Ok(LocalDriver {
                conn: Arc::new(Mutex::new(conn)),
            })
        }

        async fn with_conn<T: Send + 'static>(
            &self,
            f: impl FnOnce(&mut Connection) -> SqlResult<T> + Send + 'static,
        ) -> SqlResult<T> {
            let conn = self.conn.clone();
            tokio::task::spawn_blocking(move || {
                let mut guard = conn.lock().unwrap_or_else(|e| e.into_inner());
                f(&mut guard)
            })
            .await
            .map_err(|e| SqlError::new(format!("database task failed: {e}")))?
        }

        pub async fn query(&self, stmt: Stmt) -> SqlResult<QueryResult> {
            self.with_conn(move |c| run(c, &stmt).map_err(err)).await
        }

        pub async fn batch(&self, stmts: Vec<Stmt>) -> SqlResult<Vec<QueryResult>> {
            self.with_conn(move |c| {
                let tx = c.transaction().map_err(err)?;
                let mut out = Vec::with_capacity(stmts.len());
                for s in &stmts {
                    out.push(run(&tx, s).map_err(err)?);
                }
                tx.commit().map_err(err)?;
                Ok(out)
            })
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Turso / libSQL over HTTP (Hrana v2 pipeline)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TursoDriver {
    client: reqwest::Client,
    pipeline_url: String,
    token: Option<String>,
}

fn b64_decode(s: &str) -> Vec<u8> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    STANDARD
        .decode(s)
        .or_else(|_| STANDARD_NO_PAD.decode(s.trim_end_matches('=')))
        .unwrap_or_default()
}

fn encode_value(v: &Value) -> Json {
    match v {
        Value::Null => json!({ "type": "null" }),
        Value::Integer(i) => json!({ "type": "integer", "value": i.to_string() }),
        Value::Real(f) => json!({ "type": "float", "value": f }),
        Value::Text(s) => json!({ "type": "text", "value": s }),
        Value::Blob(b) => json!({
            "type": "blob",
            "base64": base64::engine::general_purpose::STANDARD.encode(b),
        }),
    }
}

fn decode_value(v: &Json) -> Value {
    match v.get("type").and_then(Json::as_str) {
        Some("integer") => match v.get("value") {
            Some(Json::String(s)) => s.parse().map(Value::Integer).unwrap_or(Value::Null),
            Some(Json::Number(n)) => n.as_i64().map(Value::Integer).unwrap_or(Value::Null),
            _ => Value::Null,
        },
        Some("float") => match v.get("value") {
            Some(Json::Number(n)) => Value::Real(n.as_f64().unwrap_or(0.0)),
            Some(Json::String(s)) => Value::Real(s.parse().unwrap_or(0.0)),
            _ => Value::Null,
        },
        Some("text") => Value::Text(
            v.get("value")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        Some("blob") => Value::Blob(b64_decode(
            v.get("base64").and_then(Json::as_str).unwrap_or_default(),
        )),
        _ => Value::Null,
    }
}

fn encode_stmt(s: &Stmt) -> Json {
    json!({
        "sql": s.sql,
        "args": s.args.iter().map(encode_value).collect::<Vec<_>>(),
        "want_rows": true,
    })
}

fn decode_result(r: &Json) -> QueryResult {
    let rows = r
        .get("rows")
        .and_then(Json::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    Row(row
                        .as_array()
                        .map(|cells| cells.iter().map(decode_value).collect())
                        .unwrap_or_default())
                })
                .collect()
        })
        .unwrap_or_default();
    QueryResult {
        rows,
        affected: r
            .get("affected_row_count")
            .and_then(Json::as_u64)
            .unwrap_or(0),
    }
}

fn error_message(e: &Json) -> String {
    let msg = e
        .get("message")
        .and_then(Json::as_str)
        .unwrap_or("unknown database error");
    match e.get("code").and_then(Json::as_str) {
        Some(code) => format!("{msg} ({code})"),
        None => msg.to_string(),
    }
}

impl TursoDriver {
    /// `url` may be `libsql://…`, `https://…` or `http://…` (for a local sqld).
    pub fn new(url: &str, token: Option<String>) -> SqlResult<Self> {
        let url = url.trim().trim_end_matches('/');
        let base = if let Some(rest) = url.strip_prefix("libsql://") {
            format!("https://{rest}")
        } else if url.starts_with("https://") || url.starts_with("http://") {
            url.to_string()
        } else {
            return Err(SqlError::new(
                "TURSO_DATABASE_URL must start with libsql://, https:// or http://",
            ));
        };
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| SqlError::new(format!("HTTP client error: {e}")))?;
        Ok(TursoDriver {
            client,
            pipeline_url: format!("{base}/v2/pipeline"),
            token: token.filter(|t| !t.trim().is_empty()),
        })
    }

    async fn pipeline(&self, request: Json) -> SqlResult<Json> {
        let body = json!({ "baton": null, "requests": [request, { "type": "close" }] });
        let mut req = self.client.post(&self.pipeline_url).json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SqlError::new(format!("Could not reach the database: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| SqlError::new(format!("Database response error: {e}")))?;
        if !status.is_success() {
            let detail = serde_json::from_str::<Json>(&text)
                .ok()
                .map(|j| error_message(&j))
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(SqlError::new(format!(
                "Database HTTP {}: {detail}",
                status.as_u16()
            )));
        }
        let parsed: Json = serde_json::from_str(&text)
            .map_err(|e| SqlError::new(format!("Bad database response: {e}")))?;
        let first = parsed
            .get("results")
            .and_then(Json::as_array)
            .and_then(|r| r.first())
            .cloned()
            .ok_or_else(|| SqlError::new("Empty database response"))?;
        match first.get("type").and_then(Json::as_str) {
            Some("ok") => Ok(first
                .pointer("/response/result")
                .cloned()
                .unwrap_or(Json::Null)),
            _ => Err(SqlError::new(error_message(
                first.get("error").unwrap_or(&Json::Null),
            ))),
        }
    }

    pub async fn query(&self, stmt: Stmt) -> SqlResult<QueryResult> {
        let result = self
            .pipeline(json!({ "type": "execute", "stmt": encode_stmt(&stmt) }))
            .await?;
        Ok(decode_result(&result))
    }

    /// BEGIN IMMEDIATE; s1..sn each conditioned on the previous step; COMMIT; ROLLBACK if
    /// anything failed. Same shape the official @libsql/client uses for write batches.
    pub async fn batch(&self, stmts: Vec<Stmt>) -> SqlResult<Vec<QueryResult>> {
        let n = stmts.len();
        let mut steps = vec![json!({ "stmt": { "sql": "BEGIN IMMEDIATE" } })];
        for (i, s) in stmts.iter().enumerate() {
            steps.push(json!({
                "stmt": encode_stmt(s),
                "condition": { "type": "ok", "step": i },
            }));
        }
        let commit = n + 1;
        steps.push(json!({
            "stmt": { "sql": "COMMIT" },
            "condition": { "type": "ok", "step": n },
        }));
        steps.push(json!({
            "stmt": { "sql": "ROLLBACK" },
            "condition": { "type": "not", "cond": { "type": "ok", "step": commit } },
        }));

        let result = self
            .pipeline(json!({ "type": "batch", "batch": { "steps": steps } }))
            .await?;
        let results = result
            .get("step_results")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let errors = result
            .get("step_errors")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();

        // Report the first real error (skip ROLLBACK's own outcome).
        for e in errors.iter().take(commit + 1) {
            if !e.is_null() {
                return Err(SqlError::new(error_message(e)));
            }
        }
        let committed = results.get(commit).map(|r| !r.is_null()).unwrap_or(false);
        if !committed {
            return Err(SqlError::new("Database transaction was not committed"));
        }
        Ok((1..=n)
            .map(|i| results.get(i).map(decode_result).unwrap_or_default())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_json_roundtrip() {
        for v in [
            Value::Null,
            Value::Integer(-42),
            Value::Integer(i64::MAX),
            Value::Real(1.5),
            Value::Text("hé ₱".into()),
            Value::Blob(vec![0, 1, 2, 250]),
        ] {
            assert_eq!(decode_value(&encode_value(&v)), v);
        }
        // unpadded base64 from some servers
        assert_eq!(
            decode_value(&json!({"type":"blob","base64":"AAEC+g"})),
            Value::Blob(vec![0, 1, 2, 250])
        );
    }

    #[test]
    fn url_forms() {
        let d = TursoDriver::new("libsql://shop-me.turso.io/", None).unwrap();
        assert_eq!(d.pipeline_url, "https://shop-me.turso.io/v2/pipeline");
        assert!(TursoDriver::new("shop.db", None).is_err());
    }
}
