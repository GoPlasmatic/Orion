-- Bind the config-epoch scope to the epoch it was written for.
--
-- `epoch_scope` is a sticky column: a writer sets it, and a writer that does
-- not know about it leaves whatever was there. The migration that added it
-- reasoned that "an older writer produces the empty string" — true only until
-- the first scope-aware bump. After that, a 1.3.x node bumping the epoch
-- advances the counter and leaves a *recognised* scope behind from somebody
-- else's earlier change. A connector update made by an old node then reads as
-- `definitions` on every new node, so they skip the connector-registry reload
-- and the pool eviction that update needed, and keep serving the old endpoint
-- and the old credentials until something else bumps.
--
-- `epoch_scope_at` is the epoch `epoch_scope` was written for. A reader trusts
-- the scope only when the two agree; anything else means the widest resync,
-- which is what an unattributable scope was always supposed to mean. A writer
-- that does not know this column cannot make them agree, so its bump is never
-- mistaken for a scoped one.

ALTER TABLE "config_epoch" ADD COLUMN IF NOT EXISTS "epoch_scope_at" bigint NOT NULL DEFAULT 0;
