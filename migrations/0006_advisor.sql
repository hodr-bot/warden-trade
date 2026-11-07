-- 0006_advisor: LLM advisor opinions (read-only counsel, never executed)
-- The advisor only writes rows here; nothing ever reads them to place orders.
CREATE TABLE IF NOT EXISTS advisor_opinions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES tenants (id) ON DELETE CASCADE,
    symbol TEXT NOT NULL,
    stance TEXT NOT NULL CHECK (stance IN ('bullish', 'bearish', 'neutral', 'abstained')),
    confidence DOUBLE PRECISION NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
    rationale TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_advisor_opinions_tenant_symbol_time
    ON advisor_opinions (tenant_id, symbol, created_at DESC);
