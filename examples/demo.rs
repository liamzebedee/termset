//! Generates the screenshot embedded in the README.
//!
//!     cargo run --example demo          # or: scripts/demo-screenshot.sh
//!
//! It drives the *real* UI headlessly (via the test harness) over the custom
//! full-stack workspace in `demo/termset.yml`, feeds a few sessions with
//! realistic, colourful output, and writes the rendered frame to
//! `demo/screenshot.png` — no window, no PTY, deterministic.

use std::path::Path;

use termset_cli::testkit::Harness;

/// The custom workspace shown in the demo (a typical full-stack app layout).
const WORKSPACE: &str = include_str!("../demo/termset.yml");

// Vite dev server (Frontend / web) — the selected, visible session.
const WEB_OUT: &str = concat!(
    "\x1b[1;36m\u{279C}\x1b[0m  \x1b[1;32m~/app/web\x1b[0m \x1b[1;34mgit:(\x1b[31mmain\x1b[34m)\x1b[0m npm run dev\r\n",
    "\r\n",
    "  \x1b[1;32mVITE\x1b[0m \x1b[2mv5.4.2\x1b[0m  ready in \x1b[1m412\x1b[0m ms\r\n",
    "\r\n",
    "  \x1b[32m\u{279C}\x1b[0m  \x1b[1mLocal\x1b[0m:   \x1b[36mhttp://localhost:5173/\x1b[0m\r\n",
    "  \x1b[32m\u{279C}\x1b[0m  \x1b[1mNetwork\x1b[0m: \x1b[2muse --host to expose\x1b[0m\r\n",
    "\r\n",
    "\x1b[2m9:41:07 PM\x1b[0m \x1b[36m[vite]\x1b[0m \x1b[32mhmr update\x1b[0m \x1b[2m/src/App.tsx\x1b[0m \u{26A1}\r\n",
    "\x1b[2m9:41:12 PM\x1b[0m \x1b[36m[vite]\x1b[0m \x1b[32mhmr update\x1b[0m \x1b[2m/src/ui/Nav.tsx\x1b[0m \u{2705}\r\n",
    "\x1b[1;36m\u{279C}\x1b[0m  \x1b[1;32m~/app/web\x1b[0m \u{2588}\r\n",
);

// Rust API server (Backend / api), running in the background.
const API_OUT: &str = concat!(
    "\x1b[1;32m   Compiling\x1b[0m api v0.3.1 \x1b[2m(/Users/liam/app/api)\x1b[0m\r\n",
    "\x1b[1;32m    Finished\x1b[0m \x1b[1mdev\x1b[0m profile in 3.84s\r\n",
    "\x1b[1;32m     Running\x1b[0m `target/debug/api`\r\n",
    "\x1b[32m INFO\x1b[0m api: \u{1F680} listening on \x1b[1mhttp://0.0.0.0:8080\x1b[0m\r\n",
    "\x1b[32m INFO\x1b[0m api::db: pool connected \x1b[2m(8 conns)\x1b[0m\r\n",
    "\x1b[33m WARN\x1b[0m api::auth: token cache cold, warming\u{2026}\r\n",
    "\x1b[32m INFO\x1b[0m api: \x1b[1;34mGET\x1b[0m /v1/health \x1b[32m200\x1b[0m \x1b[2m1.2ms\x1b[0m\r\n",
);

// Postgres via docker compose (Infra / postgres).
const PG_OUT: &str = concat!(
    "\x1b[1mdb-1\x1b[0m  | \x1b[32mLOG:\x1b[0m  database system is ready to accept connections\r\n",
    "\x1b[1mdb-1\x1b[0m  | \x1b[32mLOG:\x1b[0m  autovacuum launcher started\r\n",
);

fn main() {
    // A roomy logical frame so the README image is sharp at 1x.
    let mut h = Harness::with_window(WORKSPACE, 1180, 720, 1.0);

    // A few "running" sessions across the stack.
    h.feed("Backend/api", API_OUT);
    h.feed("Infra/postgres", PG_OUT);
    h.feed("Frontend/web", WEB_OUT);

    // Show the frontend dev server as the focused session.
    h.select("Frontend/web");

    let shot = h.screenshot("demo");
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("demo/screenshot.png");
    std::fs::create_dir_all(out.parent().unwrap()).expect("create demo dir");
    std::fs::copy(&shot, &out).expect("copy screenshot into demo/");
    println!("Wrote {}", out.display());
}
