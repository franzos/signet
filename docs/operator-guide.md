# Operator Guide

Running Signet has two sides: issuing licenses offline with `signet-issuer`, and (optionally) selling them with `signet-shop`. You can use the issuer on its own and hand licenses out however you like; the shop is there when you want Stripe to do the selling and fulfillment for you.

## Keys and the trust model

Each product line gets its own Ed25519 keypair. There are two keys per product:

- A root key (`keys/<product>/private.bin` + `public.bin`). Keep the private half offline; it's the ultimate authority for that product. The app that consumes licenses bakes in the public half.
- A web key (`keys/<product>/web-private.bin` + `web-public.bin`). This is the key the shop signs with, so it can live on the server. Because it's separate from the root key, you can rotate it without touching the root of trust: an app that embeds both public keys keeps verifying old and new licenses through a rotation.

Keys and the ledger are written relative to the current working directory, so run the issuer from a consistent location (an operator's laptop or an air-gapped box).

## Issuing licenses (signet-issuer)

`signet-issuer` is an offline CLI with three subcommands: `keygen`, `issue`, and `verify`.

### keygen

Generate a keypair for a product:

    signet-issuer keygen --product acme            # root keypair
    signet-issuer keygen --product acme --web       # the shop's web keypair
    signet-issuer keygen --product acme --force      # overwrite existing keys

- `--product` is required.
- `--web` writes the revocable web keypair instead of the root pair.
- `--force` is required to overwrite; without it, keygen refuses to clobber existing keys. Private keys are written `0600`.

### issue

Sign a license, print the base64 blob to stdout, and append a row to the ledger:

    signet-issuer issue \
      --product acme \
      --customer "Acme GmbH" \
      --email admin@acme.example \
      --tier business \
      --expires 2027-07-05 \
      --feature orgs --max-orgs 50 \
      --feature saml

Required: `--product`, `--customer`, `--email`, `--tier`. Optional: `--expires YYYY-MM-DD` (omit for a lifetime license; the date is taken as end-of-day UTC), `--feature` (repeatable, one per feature flag), `--max-orgs`, `--max-seats` (omit either for unlimited), `--private-key` (defaults to `keys/<product>/private.bin`), `--ledger` (defaults to `ledger/<product>.jsonl`), and `--note` (recorded in the ledger only).

The blob is the only thing on stdout; the human-readable summary goes to stderr, so `signet-issuer issue ... > license.txt` captures just the license. `--tier` is a free-form marketing label; what an app actually unlocks is driven by `--feature`, not the tier string.

The ledger (`ledger/<product>.jsonl`) is your record of what was issued: one JSON line per license with the id, customer, email, tier, expiry, features, limits, and note.

### verify

Verify a blob against a public key and print its claims as JSON (this doubles as inspect):

    signet-issuer verify --product acme "<blob>"
    echo "<blob>" | signet-issuer verify --product acme -

`--public-key` defaults to `keys/<product>/public.bin`. Pass `-` as the blob to read it from stdin. Verification failure exits with an error.

## Running the shop (signet-shop)

`signet-shop` is a single binary. It needs the web signing key for every product line in your catalog: at startup it loads `<KEYS_DIR>/<category>/web-private.bin` for each category and refuses to start if one is missing, so run `signet-issuer keygen --product <cat> --web` for each first.

### Configuration: environment

The shop reads its runtime configuration from the environment:

Required:

- `STRIPE_API_KEY` — your Stripe secret key.
- `STRIPE_WEBHOOK_SECRET` — the signing secret for the webhook endpoint.
- `DATABASE_URL` — SQLite database URL (fulfillment records live here).
- `BASE_URL` — the public URL of the shop. Must be `https://`, except `http://localhost` / `http://127.0.0.1` are allowed for local runs.

Optional (with defaults):

- `KEYS_DIR` (default `./keys`)
- `CONTENT_DIR` (default `./content`)
- `CATALOG_PATH` (default `./catalog.toml`)
- `BIND_ADDR` (default `127.0.0.1:8080`)
- `TRUST_PROXY` (default off) — set to `1`/`true` only when the shop sits behind a trusted reverse proxy, so the rate limiter reads the client IP from `X-Forwarded-For`. Never enable it when the shop is directly exposed, or clients can spoof their IP.

### catalog.toml

The catalog defines what's for sale. Its main sections:

- `[shop]` — `title`, and an optional `payment_notice` shown in the footer.
- `[[category]]` — a product line. `id` (a lowercase slug, unique, and not one of the reserved names `checkout`/`success`/`webhook`/`static`), `name`, optional `description` and `url`. The `id` selects the signing key and becomes the license's `product` claim.
- `[[sku]]` — a purchasable plan. `id`, `category` (matching a `[[category]]` id), `display_name`, `amount_cents` (> 0), `currency` (lowercase ISO, e.g. `eur`), `tier`, and `term` (`lifetime` or `<days>d`). Optional `description`, `url`, `price_label`, `features` (list), `max_orgs`, `max_seats`. `stripe_price_id` is filled in for you by `provision-stripe`.
- `[[page]]` — a content page served at `/p/<slug>` from `content/<slug>.md`; set `footer = true` to link it in the footer.
- `[[footer_link]]` — an extra footer link (`title`, `url`).
- `[analytics]` — an optional analytics script (`src` must be `https://`).

See `catalog.example.toml` for a complete, commented example.

### Content

Markdown pages live in `CONTENT_DIR` as `content/<slug>.md` and render per request, so you can edit them without a restart. The starter set (`content.example/`) includes `terms.md` and `privacy.md`, served at `/p/terms` and `/p/privacy`.

### Stripe

Once your SKUs have amounts and currencies, provision them in Stripe:

    signet-shop provision-stripe

This creates or updates the Stripe products and one-time prices for each SKU and writes the resolved `stripe_price_id` back into `catalog.toml` (preserving your comments). It's idempotent: unchanged SKUs are left alone, and a changed amount archives the old price and writes a new one.

At runtime the shop exposes a Stripe webhook at `POST /webhook` (configure this URL in your Stripe dashboard, pointing at `BASE_URL/webhook`). On a completed checkout it mints the license with that category's web key and stores it, keyed on the Stripe session id so fulfillment is idempotent (the webhook and the success page can't double-issue). Before minting it cross-checks the payment: live/test mode must match your key, and the amount and currency must match the SKU, otherwise it holds off rather than issuing.

### Email (optional)

If you want buyers emailed their license (and yourself notified of sales), add an `[email]` table to `catalog.toml` or set `SIGNET_EMAIL__*` environment variables (env wins). It supports a `lettermint` provider (a `token`) or `smtp` (`host`/`port`/`tls`/`user`/`pass`), plus the sender and operator addresses. With no provider configured, email is simply off; send failures are logged, not fatal.

### Running and deployment

`signet-shop` with no argument (or `serve`) starts the server on `BIND_ADDR`. A prebuilt container is published at `ghcr.io/franzos/signet`: mount your catalog, content, and keys, set the environment above, and put it behind a reverse proxy that terminates TLS (and set `TRUST_PROXY=1` there).
