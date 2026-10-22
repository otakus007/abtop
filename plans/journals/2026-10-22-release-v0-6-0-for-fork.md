---
title: Configure and ship v0.6.0 release for fork
date: 2026-10-22
summary: "Bumped version to 0.6.0, pointed cargo-dist and installer URLs to otakus007/abtop, and removed upstream-only Homebrew tap publishing."
---

# Configure and ship v0.6.0 release for fork

## What happened
Updated `Cargo.toml`, `Cargo.lock`, `README.md`, and `src/lib.rs` to version `0.6.0` and repository `https://github.com/otakus007/abtop` so `cargo-dist` generates `abtop-installer.ps1` and `abtop-installer.sh` pointing to fork releases. Removed upstream's `graykode/homebrew-tap` publish job from `dist-workspace.toml` and `.github/workflows/release.yml`, and guarded `.github/workflows/publish.yml` when `CARGO_REGISTRY_TOKEN` is absent.

## Next steps
Monitor the `Release` GitHub Actions workflow on tag `v0.6.0` to verify Windows (`x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`), macOS, and Linux release artifacts.

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
