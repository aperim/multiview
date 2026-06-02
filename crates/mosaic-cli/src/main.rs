//! The `mosaic` daemon/CLI entrypoint.
//!
//! Scaffold: this wires nothing yet. See `docs/` for the architecture and `docs/roadmap.md`.

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "mosaic starting (scaffold)"
    );
    println!(
        "mosaic {} — live video mosaic engine (scaffold).\nSee docs/ for the architecture; implementation in progress.",
        env!("CARGO_PKG_VERSION")
    );
}
