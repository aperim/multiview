//! Config-file watch (ADR-W020) — re-exported from
//! [`multiview_control::config_watch`].
//!
//! The watcher and its diff→apply core moved verbatim into `multiview-control`
//! (ADR-W022 §2) so the `POST /api/v1/config/revert-to-start` route can call
//! the SAME [`multiview_control::config_watch::apply_document_diff`] machinery
//! the watcher uses (the dependency arrow is `cli → control`, so the route
//! could never reach an implementation living here). This module is the
//! compatibility shim: the binary and the existing ADR-W020 integration tests
//! keep their `multiview_cli::config_watch::*` paths unchanged, and there is
//! exactly one apply implementation.

pub use multiview_control::config_watch::*;
