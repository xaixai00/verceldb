//! `shop-customer.exe` — the customer store on this computer.
//!
//! Env vars:
//!   CUSTOMER_ADDR       listen address (default 127.0.0.1:8080; 0.0.0.0:8080 for your LAN)
//!   SHOP_DB             database file (default: shop.db next to the .exe)
//!   TURSO_DATABASE_URL  use a hosted Turso database instead of shop.db (+ TURSO_AUTH_TOKEN)
//!   NO_BROWSER          set to skip opening the browser on start

use shop_core::{web, Db};
use shop_customer::{router, CustomerState};

#[tokio::main]
async fn main() {
    let db = Db::from_env()
        .await
        .unwrap_or_else(|e| web::fatal(&format!("Could not open the database: {e}")));
    let app = router(CustomerState {
        db: db.clone(),
        secure_cookies: false,
    });
    let addr = std::env::var("CUSTOMER_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    web::run(app, &addr, "JCS MotorParts — Customer Store", &db).await;
}
