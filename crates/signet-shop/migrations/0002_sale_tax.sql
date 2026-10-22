-- What each sale charged and the tax on it. Nullable: older rows have none.
ALTER TABLE licenses ADD COLUMN amount_total    INTEGER;
ALTER TABLE licenses ADD COLUMN amount_subtotal INTEGER;
ALTER TABLE licenses ADD COLUMN amount_tax      INTEGER;
ALTER TABLE licenses ADD COLUMN currency        TEXT;
-- Buyer's ISO country (VAT location evidence).
ALTER TABLE licenses ADD COLUMN country         TEXT;
ALTER TABLE licenses ADD COLUMN vat_id          TEXT;
-- 'reverse' or 'exempt'; NULL when tax was charged normally.
ALTER TABLE licenses ADD COLUMN tax_exemption   TEXT;
