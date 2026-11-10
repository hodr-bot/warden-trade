-- J3-lab: every evaluated strategy spec is journaled (tenant-scoped).
CREATE TABLE IF NOT EXISTS strategy_journal (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    spec JSONB NOT NULL,
    trades INTEGER NOT NULL DEFAULT 0,
    wins INTEGER NOT NULL DEFAULT 0,
    total_pnl DOUBLE PRECISION NOT NULL DEFAULT 0,
    max_drawdown DOUBLE PRECISION NOT NULL DEFAULT 0,
    promoted BOOLEAN NOT NULL DEFAULT FALSE,
    gate_reason TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_strategy_journal_tenant
    ON strategy_journal (tenant_id, created_at DESC);
