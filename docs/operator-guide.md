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

- `STRIPE_API_KEY` — your Stripe secret key, or a restricted key with the permissions listed under [API key](#api-key).
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
- `[tax]` — VAT / sales tax handling and the seller identity printed on receipts. See [Tax](#tax) below.
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

#### API key

`STRIPE_API_KEY` takes a secret key (`sk_`) or a restricted key (`rk_`). A restricted key needs:

- Checkout Sessions: write
- Products: write
- Prices: write
- Charges: read
- Balance: read

At startup the shop reads the account's balance once to learn whether the key belongs to live or test mode; every session's mode is compared against that.

#### Webhook endpoint

Create the endpoint in the Stripe Dashboard (Developers, Webhooks) or through the API, pointing at `BASE_URL/webhook`, with API version `2026-08-26.dahlia`. The shop's Stripe types are generated for that version and fail to parse events sent in a newer one, so the endpoint's version has to stay pinned even when the account's default version moves on. Subscribe it to:

- `checkout.session.completed`
- `checkout.session.async_payment_succeeded`
- `checkout.session.async_payment_failed`
- `checkout.session.expired`
- `charge.refunded`
- `charge.dispute.created`

Through the API, with a secret key:

    curl https://api.stripe.com/v1/webhook_endpoints \
      -u "$STRIPE_API_KEY:" \
      -d url=https://shop.example/webhook \
      -d api_version=2026-08-26.dahlia \
      -d "enabled_events[]=checkout.session.completed" \
      -d "enabled_events[]=checkout.session.async_payment_succeeded" \
      -d "enabled_events[]=checkout.session.async_payment_failed" \
      -d "enabled_events[]=checkout.session.expired" \
      -d "enabled_events[]=charge.refunded" \
      -d "enabled_events[]=charge.dispute.created"

The response's `secret` (`whsec_...`) is `STRIPE_WEBHOOK_SECRET`.

#### Fulfillment and holds

On a completed checkout the shop mints the license with that category's web key and stores it, keyed on the Stripe session id. The webhook and the success page run the same fulfillment, so a license is issued once whichever arrives first.

Before minting it checks the payment: the session's live/test mode must match the account, and the amount and currency must match the SKU. The webhook fulfils the session as read back from Stripe, not the event payload; a session Stripe doesn't return is held with reason `lookup_failed:session`. A paid session that fails a check, names an unknown SKU, has no signing key, or whose charge can't be read from Stripe (reason `lookup_failed`) is held instead: a row in the `holds` table, a notice to the operator address when mail is configured, and a "your payment is being checked" page for the buyer. A held session is never retried automatically; it waits for `release`.

Before minting the shop also reads the payment's latest charge. A refunded or disputed payment is held instead of minted, with reason `refund:<charge id>` or `dispute:charge:<charge id>` and no hold notice, and `release` refuses it.

A full refund (`charge.refunded`) or a dispute (`charge.dispute.created`) marks the session's license revoked, recording the refund's charge id or the dispute id and the time. A partial refund keeps the license. The license blob verifies offline, so revocation is a record for you to act on, not a remote switch. A refund or dispute on a held session is written onto the hold, and `release` then refuses it. A refund or dispute whose charge Stripe refuses to look up is held under `charge:<charge id>` instead of a session id, with reason `lookup_failed:permanent`, and a hold notice goes to the operator. A refund or dispute of a charge with no Checkout Session is not a sale of this shop and is acknowledged without a hold.

#### Mail outbox

License mail, operator sale notices and hold notices are written to the `mail_outbox` table in the same transaction as the license or hold, and the running server sends them in the background every 15 seconds. A failed send retries with a backoff that starts at 30 seconds and doubles up to 6 hours. After 20 failed attempts a row is dead: it is no longer sent and is counted in the `holds` output. Mail queued by `release` or `reconcile` goes out from the running server.

#### Operator commands

These read the same environment as `serve`. `holds` reads the database only; `release` and `reconcile` also load the catalog and keys and connect to Stripe. None of them starts the web server or sends mail.

    signet-shop holds [--all]

Lists open holds (with `--all`, resolved ones too) with their age, session id (or `charge:<charge id>`) and reason, the number of dead outbox rows, and every revoked license with its revocation time, session id and reason.

    signet-shop release <session_id>

Issues the license for a held session after you have checked it. The session must have an open hold that carries no refund or dispute, must be paid according to Stripe, and its latest charge must be neither refunded nor disputed: a refunded or disputed charge is written onto the hold and the release fails. The amount, currency and mode checks are skipped; an unknown SKU or a missing signing key still fails. Minting, resolving the hold and queueing the mail happen in one transaction, and the buyer's success page shows the license from then on. Releasing the same session again fails, since its hold is resolved.

    signet-shop release charge:<charge id>

Closes a `charge:` hold, which records a refund or dispute that could not be matched to a sale. It reads the database only, mints nothing and contacts no one. Before closing it, find the charge in Stripe and revoke any matching license by hand.

    signet-shop reconcile --since YYYY-MM-DD

Lists the account's paid Checkout Sessions created since midnight UTC of that date and runs each through the webhook's fulfillment, then prints how many were fulfilled, already present, held, or failed. The list leaves out sessions tagged for another service. It prints no session ids, and exits non-zero when any session failed. Run it after webhook downtime; running it twice is harmless.

#### Sharing a Stripe account

Several apps can share one Stripe account. Each tags its Checkout Sessions with `metadata.service` (`signet` for this shop) and ignores session events and sessions tagged for another service; the success page treats such a session as not found, and `reconcile` never lists them. A Checkout charge inherits the tag, so refunds of another app's Checkout sale are ignored. Disputes carry no tag and are matched to a session through the charge's payment intent: a dispute on another app's tagged session records nothing here and is acknowledged. A refund or dispute of a charge with no Checkout Session, such as a subscription invoice charge, is acknowledged and records nothing.

#### Logging

`RUST_LOG` sets the log filter; the default is `info,stripe=error`. async-stripe logs each failed request's path, Checkout session ids included, at WARN, so its logs below error are off unless `RUST_LOG` turns them on.

#### Idempotency

There is no `webhook_events` table. Every effect is idempotent on its own unique key: a license on its session id, a hold on its session id, and a revocation on the `revoked_at IS NULL` guard, which keeps the first reason. A replayed or duplicated event finds the effect already in place and changes nothing.

### Tax

Who works out VAT is a per-site choice, made in `[tax]`.

**`mode = "none"`** (the default, and what you get by omitting the section) charges the catalog amount and says nothing about tax. That is the right setting if you sell in one country, price tax-inclusive, and split it in your own books.

**`mode = "stripe"`** turns on Stripe Tax. The checkout session then asks Stripe to compute the destination rate, requires a billing address (EU rules want two non-contradictory pieces of location evidence, and this is the one the buyer types), and collects a VAT ID — which is what turns an intra-EU B2B sale into a reverse charge, since Stripe validates the number against VIES before zero-rating it.

Two things to get right before the first live sale:

- **Run `provision-stripe` after switching the mode.** It marks prices tax-inclusive, which is what EU consumer law expects (the displayed price is the final price). A price's tax behaviour can't be changed once set, so existing prices are replaced and the old ones archived.
- **Register where you have an obligation.** Stripe Tax calculates and collects; it does not register you and, on the no-subscription tier, does not file. It charges 0.5% per taxed transaction with no monthly fee, and it monitors thresholds so you find out before you cross one.

The seller fields are printed on the receipt:

```toml
[tax]
mode = "stripe"
seller_name = "Example Lda"          # required when mode = "stripe"
seller_address = "Rua Example 1, 1000-001 Lisboa, Portugal"
seller_vat_id = "PT999999990"
receipt_note = "This is a payment receipt, not a fiscal invoice."
```

### Receipts, and why they are not invoices

When `seller_name` is set, the buyer's license email carries a payment receipt: seller identity and VAT number, the net/VAT/total split, the buyer's country and VAT ID, and the reverse-charge statement where one applies. The same figures are added to the operator sale notice, since that is the mail that gets forwarded to a bookkeeper. Every sale's amounts are also stored on its row in the database, so the shop's own records can answer a VAT return without going back to Stripe.

It is deliberately a receipt and not an invoice. Portugal, among others, requires invoices to be issued by software certified by the tax authority, carrying a controlled sequential number, a hash and a QR code, with monthly SAF-T reporting — and the obligation attaches to using invoicing software at all, not just to a turnover threshold. Signet does not try to be that software. Issue the fiscal document from whatever system you already file with, and use `receipt_note` to say what this document is in your jurisdiction.

### Email (optional)

If you want buyers emailed their license (and yourself notified of sales), add an `[email]` table to `catalog.toml` or set `SIGNET_EMAIL__*` environment variables (env wins). It supports a `lettermint` provider (a `token`) or `smtp` (`host`/`port`/`tls`/`user`/`pass`), plus the sender and operator addresses. With no provider configured, email is simply off. Mail goes through the [outbox](#mail-outbox), so a send failure is retried and never blocks license delivery.

### Running and deployment

`signet-shop` with no argument (or `serve`) starts the server on `BIND_ADDR`. A prebuilt container is published at `ghcr.io/franzos/signet`: mount your catalog, content, and keys, set the environment above, and put it behind a reverse proxy that terminates TLS (and set `TRUST_PROXY=1` there).
