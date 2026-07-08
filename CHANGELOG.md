# Changelog

## [0.1.1] - 2026-07-08

### Changed
- Renamed the library crate `signet-core` to `signetlib`; imports become `signetlib::`

### Added
- `signetlib` published to crates.io on tagged releases (`cargo add signetlib`)

## [0.1.0] - 2026-07-07

### Added
- `signet-core`: signed license claims, codec, and issuing
- `signet-issuer`: CLI to mint and sign licenses
- `signet-shop`: axum storefront selling licenses via Stripe Checkout
- SQLite-backed fulfilment with idempotent, per-session license minting
- TOML-driven product catalog and markdown content pages
- Themeable templates with static asset serving and rate limiting
- Docker image and multi-arch release binaries
