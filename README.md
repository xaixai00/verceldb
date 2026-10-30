# JCS MotorParts — motorcycle parts store (Rust)

Runs two ways from the same code:

- **Online on Vercel** — one site: the store at `/`, the admin at `/admin` (password protected), data in a hosted [Turso](https://turso.tech) database. See [Deploy to Vercel](#deploy-to-vercel).
- **On a PC** — two Windows executables that share one local SQLite file:

| App | File | Opens | Who uses it |
|---|---|---|---|
| **Admin** | `shop-admin.exe` | http://127.0.0.1:8081 | The shop owner — add products, choose what's featured on the Home screen, store settings, see customers |
| **Customer** | `shop-customer.exe` | http://127.0.0.1:8080 | Customers — sign up / sign in, browse, product popup, cart |

Each `.exe` is a small web server with its UI built in. Double-click it, a console window shows the address, and your browser opens automatically. Close the console window (or press Ctrl+C) to stop it.

Both apps read and write **`shop.db` in the same folder as the .exe files**, so keep them together. Nothing needs to be installed — the database engine is compiled into the executables.

## Features

**Customer app**
- Sign-in screen with *Sign in* / *Create account*. Accounts are stored in the database (passwords hashed with Argon2id), sessions last 30 days.
- **Home** tab — the products the admin featured, in the admin's order.
- **Products** tab — every product, with search (name, description, category, motorcycle model) and category filters.
- Clicking a product opens a popup with:
  1. Main item (image, category, name)
  2. Description
  3. Sell price (updates with the chosen variation)
  4. Variations (each can have its own price)
  5. Features
  6. Available for (compatible models)
  7. Quantity + **Add to cart**
- **Cart** tab — every line with its variation, unit price and line total; **+ / −** buttons or type a quantity, remove a line, empty the cart, and *Add more items*. The cart is saved per customer in the database.

**Admin app**
- **Products** — add, edit and delete products: name, category, description, base price, main image (upload or drag-and-drop; large photos are resized), variations with optional price overrides, features, and compatible models. *Load sample products* fills an empty catalog for testing.
- **Home screen** — feature existing products on the customer Home tab, reorder them (↑ ↓) or remove them. Changes are live immediately.
- **Store settings** — shop name, tagline and currency symbol (default ₱) shown in both apps.
- **Customers** — who signed up and how many items they have in their cart.

## Deploy to Vercel

Vercel can't keep a SQLite file (its storage is temporary), so the online site stores everything in **Turso**, a hosted database built on SQLite. The free tier is plenty for a shop like this.

### 1. Create the Turso database

On https://app.turso.tech (sign up with GitHub):

1. **Create Database** → name it e.g. `jcs-motorparts`. Choose a standard **libSQL** database (not the newer "Turso Database" engine) and pick the **Singapore** location (AWS `ap-southeast-1`) if it asks — it's closest to the Philippines and matches the Vercel region in `vercel.json`.
2. Open the database and copy its **URL** (starts with `libsql://`).
3. **Create Token** (read & write, no expiry) and copy it.

Using the CLI instead (macOS/Linux/WSL): `turso db create jcs-motorparts`, then `turso db show jcs-motorparts --url` and `turso db tokens create jcs-motorparts`.

The app creates its tables automatically on the first request.

### 2. Import the project into Vercel

1. Push this folder to GitHub (include `Cargo.lock`).
2. In Vercel: **Add New → Project → Import** the repository. Leave Framework Preset as **Other** and the build settings empty — `vercel.json` handles it.
3. Before deploying, open **Environment Variables** and add (see `.env.example`):

   | Name | Value |
   |---|---|
   | `TURSO_DATABASE_URL` | the `libsql://…` URL |
   | `TURSO_AUTH_TOKEN` | the token |
   | `ADMIN_PASSWORD` | a long password for `/admin` |

4. **Deploy.** The first build compiles Rust and takes a few minutes; later builds are cached.

If you connect Turso through the Vercel Marketplace integration instead, check the variable names it adds — the app reads exactly `TURSO_DATABASE_URL` and `TURSO_AUTH_TOKEN`.

### 3. Use it

- Store: `https://<your-project>.vercel.app/`
- Admin: `https://<your-project>.vercel.app/admin` → sign in with `ADMIN_PASSWORD` → **Products → Load sample products** to try it out.

Changing an environment variable needs a **Redeploy** (Deployments → ⋯ → Redeploy) to take effect. If something's missing, the site shows a "can't start yet" page that names the problem.

### How it's wired

- `vercel.json` rewrites every path to one Rust function, `api/index.rs`, which serves the store and mounts the admin under `/admin`. It uses Vercel's Rust runtime (currently in beta) via the `vercel_runtime` crate.
- `public/` is the static output folder (just `robots.txt`), so your source files are never published.
- Admin sessions last 12 hours; wrong passwords are slowed down. Cookies are `HttpOnly`, `SameSite=Lax` and `Secure` on Vercel.
- Image uploads are limited to 3 MB (the admin page shrinks photos automatically) to stay under Vercel's 4.5 MB request limit.

### Managing the online shop from the .exe

The admin `.exe` can manage the Turso database directly — set the same variables before starting it:

```bat
set TURSO_DATABASE_URL=libsql://jcs-motorparts-yourname.turso.io
set TURSO_AUTH_TOKEN=your-token
shop-admin.exe
```

### Test the Vercel build locally

```bash
cargo run --bin index --features local     # serves http://127.0.0.1:3000 using target/debug/shop.db
```
Set `TURSO_DATABASE_URL`/`TURSO_AUTH_TOKEN` to test against Turso, and `ADMIN_PASSWORD` to unlock `/admin`.

## Building with GitHub Actions

The workflow is in `.github/workflows/build.yml`.

1. Push this folder to a GitHub repository (commit `Cargo.lock` too — the build uses `--locked`).
2. Every push to `main`/`master`, every pull request, or a manual run (*Actions → Build → Run workflow*) will:
   - run `cargo fmt --check`, `cargo clippy` and the tests on Linux (including the Turso driver against a protocol test server in `tools/`),
   - compile the Vercel function, then
   - build `shop-admin.exe` and `shop-customer.exe` on Windows.

   Vercel builds and deploys on its own whenever you push; the workflow is a safety net that catches build errors first.
3. Download them from the run page under **Artifacts → MotoShop-windows-x64**.
4. To publish a release, push a tag that starts with `v`:
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
   The workflow attaches `MotoShop-windows-x64-v0.1.0.zip` to a new GitHub Release.

## Building locally

Requires Rust (https://rustup.rs). On Windows, the MSVC build tools that rustup asks for are enough.

```bash
cargo build --release -p shop-admin -p shop-customer
# binaries: target/release/shop-admin(.exe) and target/release/shop-customer(.exe)
cargo test --workspace        # unit tests
```

During development you can run both with `cargo run -p shop-admin` and `cargo run -p shop-customer` in two terminals; they share `target/debug/shop.db`.

## Configuration (optional environment variables)

| Variable | Default | Purpose |
|---|---|---|
| `SHOP_DB` | `shop.db` next to the .exe | Use a different database file |
| `CUSTOMER_ADDR` | `127.0.0.1:8080` | Customer app address. Set `0.0.0.0:8080` to let phones/PCs on your Wi-Fi open the store at `http://<this-PC's-IP>:8080` (allow it through Windows Firewall when asked) |
| `ADMIN_ADDR` | `127.0.0.1:8081` | Admin app address. Without `ADMIN_PASSWORD` the admin only accepts requests addressed to this computer |
| `ADMIN_PASSWORD` | unset | Require this password to sign in to the admin (lets other devices use it) |
| `TURSO_DATABASE_URL` / `TURSO_AUTH_TOKEN` | unset | Use a Turso database instead of `shop.db` (e.g. to manage the online shop) |
| `STORE_URL` | derived from `CUSTOMER_ADDR` | Where the admin's *Open store* link points |
| `NO_BROWSER` | unset | Set to anything to stop the apps opening a browser on start |

Example (Command Prompt):
```bat
set CUSTOMER_ADDR=0.0.0.0:8080
shop-customer.exe
```

## Notes

- **Backups:** copy `shop.db` (and `shop.db-wal` / `shop.db-shm` if present) while the apps are closed.
- **Admin access (.exe):** with no `ADMIN_PASSWORD`, the admin is protected by only accepting this computer. Set `ADMIN_PASSWORD` before exposing it with `ADMIN_ADDR=0.0.0.0:8081`.
- **Backups (Vercel):** Turso keeps point-in-time backups; you can also export with `turso db shell jcs-motorparts .dump > backup.sql`.
- **Fonts:** the UI uses Google Fonts when online and falls back to Windows fonts offline.
- **Windows SmartScreen** may warn the first time you run an unsigned .exe — choose *More info → Run anyway*.
- Checkout/orders aren't part of this version; the cart is where the flow ends.

## Project layout

```
Cargo.toml                     workspace + the Vercel package (bin "index")
api/index.rs                   Vercel function: store at /, admin at /admin
vercel.json                    rewrites everything to api/index, region sin1
public/                        static output (robots.txt only)
crates/shop-core/              shared: database (local SQLite or Turso), auth, web helpers, base.css
crates/shop-admin/             admin router (lib.rs) + shop-admin.exe (main.rs) + assets/admin.html
crates/shop-customer/          store router (lib.rs) + shop-customer.exe (main.rs) + assets/customer.html
tools/hrana_test_server.py     Turso-protocol test server used by CI
.github/workflows/build.yml    CI: lint, tests, Vercel build check, Windows .exe build, releases
```
