-- A deployment receipt's operation id, epoch and receiving node, recorded
-- when the operation commits. Nullable: rows written before this read back
-- with the revision as the epoch and the reading node as the receiver.
ALTER TABLE "deployment_operations"
    ADD COLUMN "operation_id" text,
    ADD COLUMN "epoch" bigint,
    ADD COLUMN "receiving_node_id" text;
