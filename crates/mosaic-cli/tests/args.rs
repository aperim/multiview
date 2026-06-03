//! Argument-parsing tests for the `mosaic` binary.
//!
//! These assert the clap grammar: subcommand selection, required/positional
//! arguments, and the `run` flags (`--headless`, `--ticks`). Parsing is pure
//! and side-effect-free, so it is exercised directly via [`Cli::parse_from`].
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use clap::Parser as _;
use mosaic_cli::cli::{Cli, Command};

#[test]
fn validate_subcommand_parses_path() {
    let cli = Cli::parse_from(["mosaic", "validate", "examples/2x2.toml"]);
    match cli.command {
        Command::Validate(args) => {
            assert_eq!(args.config, PathBuf::from("examples/2x2.toml"));
        }
        other @ Command::Run(_) => panic!("expected Validate, got {other:?}"),
    }
}

#[test]
fn run_subcommand_defaults() {
    let cli = Cli::parse_from(["mosaic", "run", "examples/2x2.toml"]);
    match cli.command {
        Command::Run(args) => {
            assert_eq!(args.config, PathBuf::from("examples/2x2.toml"));
            assert!(!args.headless, "headless must default to false");
            assert_eq!(args.ticks, None, "ticks must default to None (run forever)");
        }
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn run_subcommand_headless_with_ticks() {
    let cli = Cli::parse_from([
        "mosaic",
        "run",
        "examples/2x2.toml",
        "--headless",
        "--ticks",
        "120",
    ]);
    match cli.command {
        Command::Run(args) => {
            assert!(args.headless);
            assert_eq!(args.ticks, Some(120));
        }
        other @ Command::Validate(_) => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn missing_config_is_a_parse_error() {
    let err = Cli::try_parse_from(["mosaic", "validate"]).unwrap_err();
    // clap reports the missing required positional argument.
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::MissingRequiredArgument,
        "a missing config path must be a clap usage error, not a panic"
    );
}

#[test]
fn unknown_subcommand_is_a_parse_error() {
    let err = Cli::try_parse_from(["mosaic", "frobnicate", "x"]).unwrap_err();
    assert!(matches!(
        err.kind(),
        clap::error::ErrorKind::InvalidSubcommand | clap::error::ErrorKind::UnknownArgument
    ));
}

#[test]
fn ticks_must_be_a_number() {
    let err =
        Cli::try_parse_from(["mosaic", "run", "c.toml", "--ticks", "notanumber"]).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}
