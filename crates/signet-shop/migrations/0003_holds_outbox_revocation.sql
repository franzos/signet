CREATE TABLE holds (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT    NOT NULL UNIQUE,
    reason      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    resolved_at INTEGER
);
CREATE TABLE mail_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT    NOT NULL,
    recipient       TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    sent_at         INTEGER,
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX idx_mail_outbox_due ON mail_outbox(next_attempt_at) WHERE sent_at IS NULL;
ALTER TABLE licenses ADD COLUMN revoked_at     INTEGER;
ALTER TABLE licenses ADD COLUMN revoked_reason TEXT;
