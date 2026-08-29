-- Scope the config-epoch fan-out (A11 follow-up).
--
-- `config_epoch` carried a counter and nothing about *what* changed, so every
-- node answering a bump ran the widest possible resync: reload the connector
-- registry and evict every cached SQL, MongoDB and cache pool. One workflow
-- activation therefore dropped every pooled connection on every node in the
-- fleet and made them all reconnect — a reconnect storm for a change that
-- touched no connector.
--
-- `epoch_scope` names what the bumping node changed, so a peer can reload the
-- engine without touching pools that nothing altered. Empty (and any value a
-- reader does not recognise) means the widest resync, which is what an older
-- writer produces and what keeps a rolling deploy correct.

ALTER TABLE "config_epoch" ADD COLUMN IF NOT EXISTS "epoch_scope" text NOT NULL DEFAULT '';
