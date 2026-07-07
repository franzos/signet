CREATE TABLE licenses (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    stripe_session_id TEXT    NOT NULL UNIQUE,
    license_id        TEXT    NOT NULL,
    product           TEXT    NOT NULL,
    sku               TEXT    NOT NULL,
    customer          TEXT    NOT NULL,
    email             TEXT    NOT NULL,
    blob              TEXT    NOT NULL,
    issued_at         INTEGER NOT NULL,
    created_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX idx_licenses_email ON licenses(email);
