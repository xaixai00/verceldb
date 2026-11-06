//! Shared code for the JCS MotorParts apps.
//!
//! * `sql`  – driver layer: local SQLite file or hosted Turso (libSQL over HTTP)
//! * `db`   – the shop's tables and queries, on top of `sql`
//! * `auth` – password hashing, session tokens, constant-time compare
//! * `captcha` – optional Cloudflare Turnstile human check for sign-in
//! * `web`  – error type, image/CSS handlers, cookies and the server runner

pub mod auth;
pub mod captcha;
pub mod db;
pub mod sql;
pub mod web;

pub use db::{default_db_path, Db, DbError};

/// Stylesheet shared by the admin and customer pages.
pub const BASE_CSS: &str = include_str!("../assets/base.css");

/// Brand logo, drawn faintly behind both apps (see `body::before` in base.css).
pub const LOGO_SVG: &str = include_str!("../assets/logo.svg");

/// JCS brand mark (orange), used in the top bars, sign-in screen and as the favicon.
pub const JCS_LOGO_SVG: &str = include_str!("../assets/jcs-logo.svg");
