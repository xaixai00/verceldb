//! `shop-admin.exe` — manage products, the Home screen and settings on this computer.
//!
//! Env vars:
//!   ADMIN_ADDR          listen address (default 127.0.0.1:8081)
//!   ADMIN_PASSWORD      require this password to sign in (lets other devices use it)
//!   STORE_URL           link shown for "Open store" (default from CUSTOMER_ADDR)
//!   SHOP_DB             database file (default: shop.db next to the .exe)
//!   TURSO_DATABASE_URL  manage a hosted Turso database instead (+ TURSO_AUTH_TOKEN)
//!   NO_BROWSER          set to skip opening the browser on start

use shop_admin::{router, AdminConfig};
use shop_core::{web, Db};

#[tokio::main]
async fn main() {
    let db = Db::from_env()
        .await
        .unwrap_or_else(|e| web::fatal(&format!("Could not open the database: {e}")));

    let store_url = std::env::var("STORE_URL").unwrap_or_else(|_| {
        let addr = std::env::var("CUSTOMER_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
        addr.parse()
            .map(|a| web::browser_url(&a))
            .unwrap_or_else(|_| "http://127.0.0.1:8080/".into())
    });
    let password = std::env::var("ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());

    let app = router(
        db.clone(),
        AdminConfig {
            base: String::new(),
            password,
            local_only: true,
            secure_cookies: false,
            store_url,
        },
    );
    let addr = std::env::var("ADMIN_ADDR").unwrap_or_else(|_| "127.0.0.1:8081".into());
    web::run(app, &addr, "JCS MotorParts — Admin", &db).await;
}
