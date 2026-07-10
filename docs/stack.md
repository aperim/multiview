# Platform & toolchain standards

Binding toolchain/platform standards for this repository (AGENTS.md rules 38–42).
These are the concrete values behind the rule-set's stack parameters. The
canonical naming/structure/invariants source of truth remains
[`docs/architecture/conventions.md`](architecture/conventions.md); this file
records the *toolchain and platform* standards that the governance rules
reference.

## Stack parameters

| Parameter | Value for this repo |
|-----------|---------------------|
| **Package managers** | **Cargo** (Rust workspace) is the primary; **npm** for the `web/` SPA. No other Rust or JS package manager — never mix in yarn/pnpm/bun for `web/`. |
| **Frozen install** | Rust: `cargo build --locked` / `cargo test --workspace --locked` (cargo **ignores** the lockfile without `--locked`). Web: `npm ci` (from the committed `web/package-lock.json`). |
| **Language / runtime** | **Rust 2021**, stable channel, MSRV **1.85** (raised from 1.82 by ed25519-dalek 3.0 / curve25519-dalek 5.0 edition-2024 deps — ADR-I010), pinned via [`rust-toolchain.toml`](../rust-toolchain.toml). `web/` is **TypeScript** (React 19 + Vite) under `strict` + `noUncheckedIndexedAccess` + `exactOptionalPropertyTypes`. |
| **Hosting / deploy** | Self-hosted **binary/daemon `multiview`** and **OCI container images** published to **GHCR** (`.github/workflows/docker.yml`, `ffmpeg-base.yml`, `release*.yml`). Linux (x86_64 + aarch64) and macOS (Apple Silicon + Intel). **No Windows.** No cloud SaaS hosting runtime. |
| **Secret manager** | **1Password** (`op read` → `chmod 600` temp file → `rm -f`, or `op ssh-agent`). Secrets never touch git or terminal history. |
| **AI co-author trailer** | `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>` |
| **CI** | **GitHub Actions** (`.github/workflows/`): `ci.yml` (change-classification gate → fmt/clippy/test, feature-gated clippy, AsyncAPI validation, inclusive-language, docs-sanity), `gitleaks.yml` (secret scan), plus mutation testing and `cargo deny check` (licenses + advisories). |
| **Vulnerability / advisory gate** | `cargo deny check advisories` (config: [`deny.toml`](../deny.toml)). |
| **Licence gate** | `cargo deny check licenses` (config: [`deny.toml`](../deny.toml)). Default build is LGPL-clean; `gpl-codecs`/`ndi` are off-by-default and escalate licensing (conventions §7). |
| **Build-output dirs** (gitignored + read-denied) | `target/` (Rust), `web/node_modules/`, `web/dist/`, `node_modules/`, `dist/`, `.multiview-build/`, `.ndi-sdk/`, `.memory/`. |

## Determinism

`Cargo.lock` **and** `web/package-lock.json` are committed. The toolchain is
pinned via `rust-toolchain.toml`; the Node version follows the devcontainer.
CI installs from the committed lockfiles with frozen/locked installs. No floating
version ranges in product crates beyond the workspace catalog pins.

Verified against the repository state on **2026-06-16**.
