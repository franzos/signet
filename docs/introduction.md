# Signet

Signet sells software licenses without a licensing service. It signs offline-verifiable license keys with Ed25519, and runs a single-binary web shop that sells them through Stripe Checkout. Your apps ship only the public key and verify locally; nothing phones home.

A license is a small signed blob carrying who bought it, which product line, which features, and when it expires. The app that consumes it needs a few lines to verify a signature. There's no license server to run, no runtime dependency, and no per-check network call: you keep a signing key offline, your app bakes in the matching public key, and verification is a local Ed25519 check.

The workspace is three crates:

- `signetlib` — the shared library: license claims, Ed25519 signing, and the wire format. Depend on it (or copy the decoder) to check licenses in your app.
- `signet-issuer` — an offline CLI: generate per-product keypairs, issue license blobs, verify and inspect them.
- `signet-shop` — a single-binary web shop: a storefront, Stripe Checkout, and idempotent license fulfillment, all configured from files.

These docs are split by what you're here to do:

- [Operator guide](./operator-guide.md) — issuing licenses and running the shop.
- [Integration guide](./integration-guide.md) — verifying licenses in your app with `signetlib`.

The source lives on [GitHub](https://github.com/franzos/signet). Signet is MIT-licensed.
