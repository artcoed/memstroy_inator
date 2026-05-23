//! Build-time configuration emitted by `build.rs`.
//!
//! See `build.rs` at the crate root for the full contract; in
//! summary, this module re-exports two items:
//!
//! * `default_server_url() -> String` — obfstr-decoded base URL of
//!   the assets-server the editor should talk to on first launch.
//! * `IS_CLIENT_BUILD: bool` — whether `MEMSTROY_CLIENT_BUILD=1` was
//!   set when the binary was compiled.
//!
//! Splitting this into a tiny stand-alone module (rather than a raw
//! `include!` at the crate root) keeps the rest of the codebase
//! oblivious to the `OUT_DIR` plumbing and lets us add additional
//! baked-in signals here later without touching `main.rs`.

include!(concat!(env!("OUT_DIR"), "/build_info.rs"));
