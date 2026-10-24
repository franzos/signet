# Changelog

## [0.1.6] - 2026-10-24

### Security
- tokio 1.54.1, rustls-webpki 0.103.16 and ctutils 0.4.5 (RUSTSEC-2026-0381, -0385, -0386)

## [0.1.5] - 2026-10-24

### Added
- `signet-shop dismiss` closes any open hold without issuing a license
- `signet-shop provision-stripe --dry-run` previews changes using Stripe reads only

### Changed
- Revoked licenses are no longer served on the success page or download
- Unsent mail for a revoked license is dropped
- `release` only accepts Checkout Session ids; other holds use `dismiss`
- `stripe-edge` pinned to `v0.1.3`

### Fixed
- Signed webhook events the shop can't process are held instead of rejected
- Session id is read from the slimmed `SessionCompleted` event

## [0.1.4] - 2026-10-22

### Added
- Stripe through the shared `stripe-edge` crate, pinned to `v0.1.0`
- `signet-shop holds`, `release` and `reconcile` operator commands
- Mail outbox with retries instead of send-on-insert
- Refunds and disputes revoke the license; a reversal before fulfilment holds the sale
- The charge is checked before minting, so a refunded or disputed payment is held
- Unresolvable refunds and disputes are held under `charge:<id>` and closed with `release`

### Changed
- Livemode is read from the Stripe account at startup
- Checkout tags sessions with `metadata.service`; other apps' events on a shared account are ignored
- The webhook fulfils the session as read back from Stripe, not the event payload
- Default log filter is `info,stripe=error`

### Fixed
- Underpaid, wrong-currency and wrong-mode payments are held for review instead of failing silently

## [0.1.3] - 2026-08-08

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
