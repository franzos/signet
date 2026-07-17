# Changelog

## [Unreleased]

### Added
- Separate revocable web signing keys: `signet-issuer keygen --web` mints `web-{private,public}.bin`; the shop now loads only the web key, so the root key stays offline and a shop compromise is fixed by rotating the web key
- `signetlib`: `decode_and_verify_any` verifies a license against multiple public keys (root + web)
- Weekly scheduled CI run of `cargo deny check advisories` that fails on new advisories

### Changed
- Fulfillment cross-checks the paid amount, currency, and livemode against the catalog SKU before issuing a license
- Buyer-typed company/name fields are sanitized (control characters stripped, capped) before reaching emails and signed claims
- License verification uses strict Ed25519 signature checking and rejects oversized blobs before parsing
- Catalog validation rejects non-positive prices and unreasonable term lengths
- Docker image runs as a non-root user
- Buyer purchase emails retry once on transient failure and log loudly on permanent failure

### Fixed
- `BASE_URL` with a trailing slash no longer breaks the post-payment redirect
- `keygen` no longer leaves a private key with loose permissions when overwriting with `--force`
- Ledger files are created readable only by the owner (0600)
- Stripe webhook payloads (buyer PII) are kept out of logs

## [0.1.2] - 2026-07-09

### Added
- Optional purchase emails: buyer license and operator sale notice (SMTP or Lettermint)

### Changed
- Shop requires an https `BASE_URL` (plain http allowed for localhost)
- `signetlib` reports failures through its own error type instead of `anyhow`

### Fixed
- Unknown or expired checkout sessions show a "not found" page
- Rate limiter prunes stale per-IP state

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
