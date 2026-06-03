//! The `mosaic validate` subcommand: load a config, run
//! [`MosaicConfig::validate`] (grid-solve + cross-references), and render a
//! clear human report.
//!
//! [`validate_config`] is total: a missing file, malformed TOML, or a failed
//! semantic invariant all produce a [`ValidationReport`] whose
//! [`ValidationReport::is_ok`] is `false` and whose [`ValidationReport::render`]
//! explains *why*. It returns `Err` only for an unexpected internal fault, so
//! the binary can always print a report and pick an exit code from the
//! report's status.
use std::path::{Path, PathBuf};

use mosaic_config::MosaicConfig;

/// The outcome of validating one configuration document.
///
/// Carries the resolved path, the pass/fail status, and — on failure — the
/// reason. [`ValidationReport::render`] turns it into the text the binary
/// prints; [`ValidationReport::is_ok`] drives the process exit code.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ValidationReport {
    /// The config path that was validated.
    pub path: PathBuf,
    /// The validation outcome.
    pub status: ValidationStatus,
}

/// Whether a config validated, and if not, the human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidationStatus {
    /// The document parsed and passed every semantic invariant, plus a short
    /// summary of what was validated (canvas, cell/source/output counts).
    Ok {
        /// One-line summary of the validated document.
        summary: String,
    },
    /// The file could not be read (e.g. missing / permission denied).
    Unreadable {
        /// The underlying read error, rendered.
        reason: String,
    },
    /// The file was read but is not a valid config (parse or semantic error).
    Invalid {
        /// The first violated invariant or parse error, rendered.
        reason: String,
    },
}

impl ValidationReport {
    /// Whether the document validated cleanly.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self.status, ValidationStatus::Ok { .. })
    }

    /// Render the report as the multi-line text the binary prints.
    #[must_use]
    pub fn render(&self) -> String {
        let path = self.path.display();
        match &self.status {
            ValidationStatus::Ok { summary } => {
                format!("OK   {path}\n     {summary}")
            }
            ValidationStatus::Unreadable { reason } => {
                format!("FAIL {path}\n     could not read file: {reason}")
            }
            ValidationStatus::Invalid { reason } => {
                format!("FAIL {path}\n     invalid configuration: {reason}")
            }
        }
    }
}

/// Load and validate the configuration document at `path`.
///
/// Reads the file, parses it as TOML into a [`MosaicConfig`], and runs
/// [`MosaicConfig::validate`] (unique ids, cell↔source bindings, output codecs,
/// grid solve, and the solved [`mosaic_core::layout::Layout`]'s structural
/// check). Every failure mode — unreadable file, malformed TOML, a violated
/// invariant — is captured in the returned [`ValidationReport`] rather than
/// surfaced as an error, so the caller can print one consistent report.
///
/// # Errors
///
/// Currently infallible at the `Result` layer (all failures are reported in the
/// [`ValidationReport`]); the [`anyhow::Result`] wrapper reserves room for an
/// unexpected internal fault and keeps the app-boundary signature uniform.
pub fn validate_config(path: &Path) -> anyhow::Result<ValidationReport> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            return Ok(ValidationReport {
                path: path.to_path_buf(),
                status: ValidationStatus::Unreadable {
                    reason: err.to_string(),
                },
            });
        }
    };

    let config = match MosaicConfig::load_from_toml(&text) {
        Ok(config) => config,
        Err(err) => {
            return Ok(ValidationReport {
                path: path.to_path_buf(),
                status: ValidationStatus::Invalid {
                    reason: format!("parse: {err}"),
                },
            });
        }
    };

    match config.validate() {
        Ok(()) => Ok(ValidationReport {
            path: path.to_path_buf(),
            status: ValidationStatus::Ok {
                summary: summarize(&config),
            },
        }),
        Err(err) => Ok(ValidationReport {
            path: path.to_path_buf(),
            status: ValidationStatus::Invalid {
                reason: err.to_string(),
            },
        }),
    }
}

/// A one-line human summary of a validated document.
fn summarize(config: &MosaicConfig) -> String {
    let cadence = config.canvas.fps.rational();
    format!(
        "{}x{} @ {}/{} fps, {} source(s), {} cell(s), {} output(s)",
        config.canvas.width,
        config.canvas.height,
        cadence.num,
        cadence.den,
        config.sources.len(),
        config.cells.len(),
        config.outputs.len(),
    )
}
