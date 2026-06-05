//! Developer automation for the Multiview workspace: `cargo xtask <task>`.
//!
//! This is a developer-facing command-line tool (not engine or data-plane code), so
//! it legitimately writes to stdout/stderr and sets a process exit code. The
//! workspace-wide bans on those are relaxed here with justified `#[allow]`s.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    // reason: xtask is a dev CLI whose entire job is to report to the terminal.
)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use utoipa::OpenApi;

/// Relative path (from the workspace root) of the generated `OpenAPI` document.
const OPENAPI_OUT: &str = "docs/api/openapi.json";

/// Relative path (from the workspace root) of the generated `AsyncAPI` document.
const ASYNCAPI_OUT: &str = "docs/api/asyncapi.json";

fn main() -> ExitCode {
    let task = std::env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    match task.as_str() {
        "help" => {
            println!("xtask — Multiview developer automation");
            println!("  gen-openapi    write the OpenAPI 3.1 document to {OPENAPI_OUT}");
            println!("  gen-asyncapi   write the AsyncAPI 3.0 document to {ASYNCAPI_OUT}");
            ExitCode::SUCCESS
        }
        "gen-openapi" => match gen_openapi() {
            Ok(path) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("gen-openapi failed: {err}");
                ExitCode::FAILURE
            }
        },
        "gen-asyncapi" => match gen_asyncapi() {
            Ok(path) => {
                println!("wrote {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("gen-asyncapi failed: {err}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("unknown task: {other}");
            ExitCode::from(2)
        }
    }
}

/// Serialize `multiview-control`'s utoipa [`OpenApi`] document to pretty JSON and
/// write it to `docs/api/openapi.json` (relative to the workspace root),
/// creating the directory if needed. Returns the path written.
fn gen_openapi() -> Result<PathBuf, GenError> {
    let doc = multiview_control::openapi::ApiDoc::openapi();
    let json = doc.to_pretty_json().map_err(GenError::Serialize)?;

    let out = workspace_root().join(OPENAPI_OUT);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|source| GenError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // Write a trailing newline so the file is POSIX-clean and diff-friendly.
    let mut contents = json;
    contents.push('\n');
    std::fs::write(&out, contents).map_err(|source| GenError::Write {
        path: out.clone(),
        source,
    })?;
    Ok(out)
}

/// Generate the `AsyncAPI` 3.0 document from [`multiview_events::asyncapi`] and
/// write it to `docs/api/asyncapi.json` (relative to the workspace root),
/// creating the directory if needed. Returns the path written.
///
/// The output is deterministic (re-running yields an identical file); the CI
/// drift-gate regenerates and fails on any diff (ADR-RT006 Decision).
fn gen_asyncapi() -> Result<PathBuf, GenError> {
    let contents = multiview_events::asyncapi::generate_asyncapi_document();

    let out = workspace_root().join(ASYNCAPI_OUT);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|source| GenError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // `generate_asyncapi_document` already appends a trailing newline.
    std::fs::write(&out, contents).map_err(|source| GenError::Write {
        path: out.clone(),
        source,
    })?;
    Ok(out)
}

/// The workspace root, derived from this crate's manifest dir (`xtask/`) at
/// compile time so the task can be run from any working directory.
fn workspace_root() -> &'static Path {
    // CARGO_MANIFEST_DIR is `<root>/xtask`; the workspace root is its parent.
    let xtask_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask_dir.parent().unwrap_or(xtask_dir)
}

/// Failure modes of `gen-openapi`.
#[derive(Debug)]
enum GenError {
    /// The `OpenAPI` document could not be serialized to JSON.
    Serialize(serde_json::Error),
    /// The output directory could not be created.
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The output file could not be written.
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(source) => write!(f, "serializing OpenAPI to JSON: {source}"),
            Self::CreateDir { path, source } => {
                write!(f, "creating directory {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(f, "writing {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for GenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(source) => Some(source),
            Self::CreateDir { source, .. } | Self::Write { source, .. } => Some(source),
        }
    }
}
