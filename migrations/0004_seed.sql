-- 0004_seed: default solo-admin tenant (paper trading only, no secrets)
INSERT INTO tenants (id, name)
VALUES ('00000000-0000-0000-0000-000000000001', 'solo-admin')
ON CONFLICT (id) DO NOTHING;
