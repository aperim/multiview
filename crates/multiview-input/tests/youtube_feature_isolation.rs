//! Feature-isolation guard for the `youtube` resolver (ADR-0015 / IN-7).
//!
//! The `youtube` feature is the **pure-Rust** `yt-dlp` resolver (info-dict parse,
//! live-status classification, `expire`-deadline parse, the `tokio::process` spawn
//! shell, and the off-data-plane re-resolution loop). None of that touches libav:
//! the resolved `*.googlevideo.com` HLS master is handed *verbatim* to the
//! `ffmpeg`-gated HLS ingest path, which the CLI enables via its **own** `ffmpeg`
//! feature — the resolver never links or compiles against libav.
//!
//! So `cargo clippy -p multiview-input --features youtube` must lint on a
//! libav-free runner (the `feature-gated clippy (multiview-input, youtube)` CI
//! leg). If the `youtube` feature ever re-acquires a transitive `ffmpeg` edge,
//! `ffmpeg-sys-next`'s build script panics there (no `libavutil.pc`) and that CI
//! leg goes red. This test pins the boundary: building with `--features youtube`
//! must NOT enable this crate's `ffmpeg` feature.
#![cfg(feature = "youtube")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

/// Enabling `youtube` must not transitively enable `ffmpeg` in `multiview-input`.
///
/// `cfg!(feature = "ffmpeg")` is evaluated at compile time for *this* test binary,
/// which is built with exactly `--features youtube` on the libav-free CI leg. The
/// pure resolver carries no libav dependency, so the only way this assertion can
/// fail is a Cargo.toml feature edge re-coupling `youtube` to `ffmpeg` — the exact
/// regression that breaks the libav-free lint runner.
#[test]
fn youtube_feature_does_not_enable_ffmpeg() {
    assert!(
        !cfg!(feature = "ffmpeg"),
        "the `youtube` feature must stay libav-free: it must NOT transitively \
         enable multiview-input's `ffmpeg` feature, or the libav-free \
         `feature-gated clippy (multiview-input, youtube)` CI leg fails when \
         ffmpeg-sys-next's build script panics with no libavutil.pc",
    );
}
