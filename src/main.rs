//! Thin binary shim. All logic lives in the library crate (`termset_cli`)
//! so the integration harness in `testkit` can drive it headlessly.

fn main() {
    termset_cli::run();
}
