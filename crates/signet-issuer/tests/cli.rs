//! CLI integration tests. Each runs in a fresh tempdir because the binary
//! writes `keys/` and `ledger/` relative to the current working directory.

use std::path::Path;

use assert_cmd::Command;
use tempfile::{tempdir, TempDir};

fn issuer() -> Command {
    Command::cargo_bin("signet-issuer").unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn keygen(dir: &TempDir, product: &str) {
    issuer()
        .current_dir(dir.path())
        .args(["keygen", "--product", product])
        .assert()
        .success();
}

/// Issue a license in `dir` and return the base64 blob printed to stdout.
fn issue_blob(dir: &TempDir, extra: &[&str]) -> String {
    let mut args = vec![
        "issue",
        "--product",
        "acme",
        "--customer",
        "Acme GmbH",
        "--email",
        "a@acme.example",
        "--tier",
        "business",
    ];
    args.extend_from_slice(extra);
    let out = issuer()
        .current_dir(dir.path())
        .args(&args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap().trim().to_string()
}

#[test]
fn keygen_writes_keypair_with_restricted_private() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");

    let priv_path = dir.path().join("keys/acme/private.bin");
    let pub_path = dir.path().join("keys/acme/public.bin");
    assert!(priv_path.exists());
    #[cfg(unix)]
    assert_eq!(mode(&priv_path), 0o600);
    assert_eq!(std::fs::metadata(&pub_path).unwrap().len(), 32);
}

#[test]
fn keygen_refuses_overwrite_without_force() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");

    let out = issuer()
        .current_dir(dir.path())
        .args(["keygen", "--product", "acme"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("already exists"));

    issuer()
        .current_dir(dir.path())
        .args(["keygen", "--product", "acme", "--force"])
        .assert()
        .success();
}

#[test]
fn keygen_web_writes_web_named_keys() {
    let dir = tempdir().unwrap();
    issuer()
        .current_dir(dir.path())
        .args(["keygen", "--product", "acme", "--web"])
        .assert()
        .success();
    assert!(dir.path().join("keys/acme/web-private.bin").exists());
    assert!(dir.path().join("keys/acme/web-public.bin").exists());
}

#[test]
fn issue_prints_blob_and_appends_ledger() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");

    let out = issuer()
        .current_dir(dir.path())
        .args([
            "issue",
            "--product",
            "acme",
            "--customer",
            "Acme GmbH",
            "--email",
            "a@acme.example",
            "--tier",
            "business",
            "--feature",
            "orgs",
            "--expires",
            "2027-07-05",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].is_empty());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Issued license"));
    assert!(!stdout.contains("Issued license"));

    let ledger = dir.path().join("ledger/acme.jsonl");
    let contents = std::fs::read_to_string(&ledger).unwrap();
    assert_eq!(contents.lines().count(), 1);
    #[cfg(unix)]
    assert_eq!(mode(&ledger), 0o600);
}

#[test]
fn issue_lifetime_when_no_expiry() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");

    let out = issuer()
        .current_dir(dir.path())
        .args([
            "issue",
            "--product",
            "acme",
            "--customer",
            "Acme GmbH",
            "--email",
            "a@acme.example",
            "--tier",
            "business",
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("lifetime"));
}

#[test]
fn issue_rejects_bad_expiry() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");

    let out = issuer()
        .current_dir(dir.path())
        .args([
            "issue",
            "--product",
            "acme",
            "--customer",
            "Acme GmbH",
            "--email",
            "a@acme.example",
            "--tier",
            "business",
            "--expires",
            "not-a-date",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("date"));
}

#[test]
fn verify_roundtrips_issued_blob() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");
    let blob = issue_blob(&dir, &["--feature", "orgs"]);

    let out = issuer()
        .current_dir(dir.path())
        .args(["verify", "--product", "acme", &blob])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = String::from_utf8(out).unwrap();
    assert!(json.contains("Acme GmbH"));
    assert!(json.contains("acme"));

    let out = issuer()
        .current_dir(dir.path())
        .args(["verify", "--product", "acme", "-"])
        .write_stdin(blob)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("Acme GmbH"));
}

#[test]
fn verify_rejects_tampered_blob() {
    let dir = tempdir().unwrap();
    keygen(&dir, "acme");
    let blob = issue_blob(&dir, &[]);

    let mut bytes = blob.into_bytes();
    let mid = bytes.len() / 2;
    bytes[mid] = if bytes[mid] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(bytes).unwrap();

    let out = issuer()
        .current_dir(dir.path())
        .args(["verify", "--product", "acme", &tampered])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("verification failed"));
}
