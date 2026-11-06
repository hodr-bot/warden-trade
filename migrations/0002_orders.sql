-- 0002_orders: paper_orders (multi-tenant, paper trading only, no secrets)
CREATE TABLE IF NOT EXISTS paper_orders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    exchange TEXT NOT NULL,
    symbol TEXT NOT NULL,
    side TEXT NOT NULL,
    qty DOUBLE PRECISION NOT NULL CHECK (qty > 0),
    price DOUBLE PRECISION NOT NULL CHECK (price >= 0),
    status TEXT NOT NULL DEFAULT 'filled',
    idempotency_key TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_paper_orders_tenant_created ON paper_orders (tenant_id, created_at);
