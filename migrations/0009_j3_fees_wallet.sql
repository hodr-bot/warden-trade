-- 0009_j3: per-fill fees (J3 cost parity) + per-tenant watch addresses.
-- Paper fills now record their fee so the dashboard shows real costs
-- instead of hardcoded "0" (audit J3). Older fills predate the column:
-- NULL means unknown (treated as 0, never backfilled with fiction).
ALTER TABLE paper_orders
    ADD COLUMN IF NOT EXISTS fee DOUBLE PRECISION NOT NULL DEFAULT 0
        CHECK (fee >= 0);

-- Watch-only addresses belong to tenants, not to the process env.
-- Seeded once at boot from WALLET_WATCH_ADDRESS onto the solo tenant
-- when the table is empty (migration path); new tenants start empty so
-- one tenant's address is never visible to another (audit J3).
CREATE TABLE IF NOT EXISTS wallet_watch_addresses (
    tenant_id UUID PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
    address TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
