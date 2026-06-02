//! Developer automation for the Mosaic workspace: `cargo xtask <task>`.

fn main() {
    let task = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "help".to_string());
    match task.as_str() {
        "help" => {
            println!("xtask — Mosaic developer automation");
            println!("  (tasks coming soon: build-web, gen-openapi, gen-asyncapi, ...)");
        }
        other => {
            eprintln!("unknown task: {other}");
            std::process::exit(2);
        }
    }
}
