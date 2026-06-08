# AGENTS.md

Guidance for AI agents and contributors working in this repo.

## Naming — read this first

This project uses three distinct names. Keep them straight; do not collapse them.

| Name           | What it is                          | Where it belongs                                                                 |
| -------------- | ----------------------------------- | -------------------------------------------------------------------------------- |
| **manyterm**   | The **product name**                | All user-facing text: window title, `.desktop` `Name=`, README, docs, UI copy    |
| **mtm**        | The CLI binary / command            | `cargo run --bin mtm`, the `[[bin]]` in `Cargo.toml`, usage examples (`$ mtm …`)  |
| **termem**     | Legacy internal codename            | Library crate `termem_demo`, the WM class / `StartupWMClass`, asset filenames, the repo path |

Rules:

- **Use "manyterm" for the product** anywhere a human reads it. New user-facing
  strings must say "manyterm", never "termem".
- **`mtm` is the command** and stays `mtm` (it's short for manyterm). Don't rename it.
- **"termem" is an internal codename only.** The library crate is deliberately kept
  as `termem_demo` (see the comment in `Cargo.toml`). The Linux WM class is `"termem"`
  and must stay in sync across three places — `src/lib.rs` (`with_name`),
  `termem.desktop` (`StartupWMClass`), and `scripts/install-icon.sh` — so the
  launcher icon binds to the live window. Don't change one without the others.
- The repo lives in a directory called `termem/`; paths in `.mtm`, `.mtm.yaml`,
  `workspace`, and the `.desktop` `Exec=`/`Path=` are real filesystem paths — leave them.

## Layout

- `src/lib.rs` — all application logic (library crate `termem_demo`).
- `src/main.rs` — thin binary shim that calls `termem_demo::run()`.
- `src/testkit.rs` — headless screenshot harness used by tests/examples.
- `examples/demo.rs`, `tests/screenshots.rs` — drive the app headlessly.
- `scripts/install-icon.sh` — installs the icon + `.desktop` launcher (idempotent).
- `termem.svg` — the app icon (green terminal panel, light-blue `>_` prompt).

## Common commands

```sh
cargo build                 # build lib + the mtm binary
cargo run --bin mtm <file>  # run manyterm on a workspace file
cargo test                  # run tests (includes the screenshot harness)
cargo run --example demo    # headless demo / screenshot
bash scripts/install-icon.sh  # (re)install icon + launcher on GNOME
```
