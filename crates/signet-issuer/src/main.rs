//! `signet-issuer`: offline issuer + verifier for per-product licenses.
//!
//! Holds a per-product private signing key on disk (`keys/<product>/`); the
//! app only ever sees the matching public key (baked in at build time as
//! `pubkey.bin`). Every issued license is appended to `ledger/<product>.jsonl`
//! so future-you can reconstruct what was sold, to whom, and when, even if the
//! customer lost the blob.
//!
//! See the project README for the overall design.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use chrono::{TimeZone, Utc};
use clap::{Parser, Subcommand};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::Serialize;

use signet_core::claims::Claims;
use signet_core::codec;

#[derive(Parser, Debug)]
#[command(
    name = "signet-issuer",
    about = "Offline per-product license issuer",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate a fresh Ed25519 keypair. Writes the 32-byte raw private key
    /// and matching public key to disk. The public key file is what gets
    /// baked into your application's license verifier.
    Keygen {
        /// Product this keypair belongs to (e.g. "acme", "globex").
        /// Keys are written to `keys/<product>/{private,public}.bin`.
        #[arg(long)]
        product: String,
        /// Overwrite existing key files. Without this, keygen refuses to
        /// clobber to prevent accidental key loss.
        #[arg(long)]
        force: bool,
    },
    /// Issue a license. Signs the claim set, prints the base64 blob to
    /// stdout, and appends a JSONL ledger row to `ledger/<product>.jsonl`.
    Issue {
        /// Product this license is for (e.g. "acme", "globex"). Selects
        /// the signing key + ledger and is stamped into the claims.
        #[arg(long)]
        product: String,
        /// Path to the 32-byte raw private key. Defaults to
        /// `keys/<product>/private.bin`.
        #[arg(long)]
        private_key: Option<PathBuf>,
        /// Ledger file. Defaults to `ledger/<product>.jsonl`. Created if
        /// absent; appended to otherwise.
        #[arg(long)]
        ledger: Option<PathBuf>,
        #[arg(long)]
        customer: String,
        #[arg(long)]
        email: String,
        /// Marketed tier name (e.g. "business"). Free-form on the wire:
        /// the app carries it as a label only and gates features off
        /// the `--feature` set, not the tier. Pick whatever you want
        /// surfaced on the activation page.
        #[arg(long)]
        tier: String,
        /// Expiry date as `YYYY-MM-DD`. Omit for a lifetime license.
        #[arg(long)]
        expires: Option<String>,
        /// Repeatable. Feature flag(s) this license unlocks (e.g.
        /// `--feature orgs --feature saml`).
        #[arg(long = "feature")]
        features: Vec<String>,
        /// Max number of orgs. Omit for unlimited.
        #[arg(long)]
        max_orgs: Option<u32>,
        /// Max number of seats. Omit for unlimited.
        #[arg(long)]
        max_seats: Option<u32>,
        /// Free-form note appended to the ledger row.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Verify a license blob against a public key. Useful when a customer
    /// reports activation failure: confirms the blob is well-formed and
    /// signed by the key the app expects.
    Verify {
        /// Product to verify against (e.g. "acme", "globex"). Selects
        /// the public key.
        #[arg(long)]
        product: String,
        /// Path to the 32-byte raw public key. Defaults to
        /// `keys/<product>/public.bin`.
        #[arg(long)]
        public_key: Option<PathBuf>,
        /// The base64 blob. Pass `-` to read from stdin.
        blob: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Keygen { product, force } => cmd_keygen(&product, force),
        Cmd::Issue {
            product,
            private_key,
            ledger,
            customer,
            email,
            tier,
            expires,
            features,
            max_orgs,
            max_seats,
            note,
        } => cmd_issue(
            product,
            private_key,
            ledger,
            customer,
            email,
            tier,
            expires,
            features,
            max_orgs,
            max_seats,
            note,
        ),
        Cmd::Verify {
            product,
            public_key,
            blob,
        } => cmd_verify(&product, public_key, &blob),
    }
}

fn cmd_keygen(product: &str, force: bool) -> anyhow::Result<()> {
    let out_dir = Path::new("keys").join(product);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;

    let priv_path = out_dir.join("private.bin");
    let pub_path = out_dir.join("public.bin");
    if (priv_path.exists() || pub_path.exists()) && !force {
        bail!(
            "{} or {} already exists; pass --force to overwrite (DANGEROUS — invalidates every license signed with the old key)",
            priv_path.display(),
            pub_path.display()
        );
    }

    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key();

    write_restricted(&priv_path, signing.to_bytes().as_ref())?;
    std::fs::write(&pub_path, verifying.to_bytes())
        .with_context(|| format!("write {}", pub_path.display()))?;

    println!("Wrote private key: {}", priv_path.display());
    println!("Wrote public key:  {}", pub_path.display());
    println!();
    println!(
        "Copy {} into your application's license verifier and rebuild.",
        pub_path.display()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_issue(
    product: String,
    private_key: Option<PathBuf>,
    ledger: Option<PathBuf>,
    customer: String,
    email: String,
    tier: String,
    expires: Option<String>,
    features: Vec<String>,
    max_orgs: Option<u32>,
    max_seats: Option<u32>,
    note: String,
) -> anyhow::Result<()> {
    let private_key =
        private_key.unwrap_or_else(|| Path::new("keys").join(&product).join("private.bin"));
    let ledger = ledger.unwrap_or_else(|| Path::new("ledger").join(format!("{product}.jsonl")));
    let signing = codec::load_signing_key(&private_key)?;

    let now = Utc::now();
    let expires_at = match expires {
        None => None,
        Some(s) => {
            let date = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d")
                .with_context(|| format!("parse --expires {s} (want YYYY-MM-DD)"))?;
            // End of day in UTC to give the customer the full final day.
            let dt = date
                .and_hms_opt(23, 59, 59)
                .ok_or_else(|| anyhow!("invalid expiry date"))?;
            Some(Utc.from_utc_datetime(&dt).timestamp())
        }
    };

    let issued = signet_core::issue(
        signet_core::IssueParams {
            product: product.clone(),
            customer: customer.clone(),
            email: email.clone(),
            tier: tier.clone(),
            expires_at,
            features: features.clone(),
            max_orgs,
            max_seats,
            note: note.clone(),
        },
        now.timestamp(),
        &signing,
    )?;
    let claims = issued.claims;
    let blob = issued.blob;

    append_ledger(&ledger, &LedgerRow::from(&claims))?;

    println!("{blob}");
    eprintln!();
    eprintln!(
        "Issued license {} for {} <{}>",
        claims.license_id, customer, email
    );
    eprintln!("  product:    {product}");
    eprintln!("  tier:       {tier}");
    eprintln!(
        "  expires:    {}",
        match expires_at {
            Some(ts) => Utc
                .timestamp_opt(ts, 0)
                .single()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| ts.to_string()),
            None => "lifetime".to_string(),
        }
    );
    eprintln!("  features:   {}", features.join(", "));
    eprintln!(
        "  max_orgs:   {}",
        max_orgs
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unlimited".into())
    );
    eprintln!(
        "  max_seats:  {}",
        max_seats
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unlimited".into())
    );
    eprintln!("  ledger:     {}", ledger.display());
    Ok(())
}

fn cmd_verify(product: &str, public_key: Option<PathBuf>, blob: &str) -> anyhow::Result<()> {
    let public_key =
        public_key.unwrap_or_else(|| Path::new("keys").join(product).join("public.bin"));
    let verifying = codec::load_verifying_key(&public_key)?;
    let raw = if blob == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        blob.to_string()
    };
    let claims = codec::decode_and_verify(raw.trim(), &verifying)
        .map_err(|e| anyhow!("verification failed: {e}"))?;
    println!("{}", serde_json::to_string_pretty(&claims)?);
    Ok(())
}

#[derive(Serialize)]
struct LedgerRow<'a> {
    issued_at: String,
    license_id: &'a str,
    customer: &'a str,
    email: &'a str,
    product: &'a str,
    tier: &'a str,
    expires_at: Option<String>,
    features: &'a [String],
    max_orgs: Option<u32>,
    max_seats: Option<u32>,
    note: &'a str,
}

impl<'a> From<&'a Claims> for LedgerRow<'a> {
    fn from(c: &'a Claims) -> Self {
        Self {
            issued_at: Utc
                .timestamp_opt(c.issued_at, 0)
                .single()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| c.issued_at.to_string()),
            license_id: &c.license_id,
            customer: &c.customer,
            email: &c.email,
            product: &c.product,
            tier: &c.tier,
            expires_at: c
                .expires_at
                .and_then(|ts| Utc.timestamp_opt(ts, 0).single().map(|d| d.to_rfc3339())),
            features: &c.features,
            max_orgs: c.max_orgs,
            max_seats: c.max_seats,
            note: &c.note,
        }
    }
}

fn append_ledger(path: &Path, row: &LedgerRow) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open ledger {}", path.display()))?;
    let line = serde_json::to_string(row)?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Write a file with 0o600 perms on Unix (private key hygiene). On other
/// platforms falls back to a plain write.
fn write_restricted(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("open {} for write", path.display()))?;
    f.write_all(contents)?;
    Ok(())
}
