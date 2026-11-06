-- 0005_risk_limits: per-tenant risk configuration (paper trading only, no secrets)
-- Risk limits are read by the risk engine at order time. Agents and API
-- callers can NEVER write these rows: only a direct DB operator changes
-- them. This keeps the independence guarantee:
-- "Les agents ne peuvent jamais modifier ou désactiver leurs propres limites."
CREATE TABLE IF NOT EXISTS risk_limits (
    tenant_id UUID PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    max_order_notional DOUBLE PRECISION NOT NULL DEFAULT 10000.0 CHECK (max_order_notional > 0),
    max_exposure_per_asset DOUBLE PRECISION NOT NULL DEFAULT 50000.0 CHECK (max_exposure_per_asset > 0),
    max_daily_loss DOUBLE PRECISION NOT NULL DEFAULT 1000.0 CHECK (max_daily_loss > 0),
    max_open_orders INTEGER NOT NULL DEFAULT 10 CHECK (max_open_orders > 0),
    kill_switch BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed conservative defaults for the solo-admin tenant (same id as 0004_seed).
INSERT INTO risk_limits (tenant_id)
VALUES ('00000000-0000-0000-0000-000000000001')
ON CONFLICT (tenant_id) DO NOTHING;
