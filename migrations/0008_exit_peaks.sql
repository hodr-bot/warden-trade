-- Trailing-stop peaks survive restarts (J2: `trailing static perdu`).
--
-- The in-memory peak per symbol is the reference for the trailing stop.
-- On restart it re-seeds from here instead of from the entry average,
-- so a rally built before a deploy is not forgotten. Rows are deleted
-- when the position exits (peak back to 0). Scoped per tenant like
-- every other trading table; agents never touch it directly.
CREATE TABLE IF NOT EXISTS exit_peaks (
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    symbol TEXT NOT NULL,
    peak DOUBLE PRECISION NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (tenant_id, symbol)
);
