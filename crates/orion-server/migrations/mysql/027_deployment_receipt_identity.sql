-- A deployment receipt's operation id, epoch and receiving node, recorded
-- when the operation commits. Nullable: rows written before this read back
-- with the revision as the epoch and the reading node as the receiver.
ALTER TABLE `deployment_operations`
    ADD COLUMN `operation_id` varchar(128) NULL,
    ADD COLUMN `epoch` bigint NULL,
    ADD COLUMN `receiving_node_id` varchar(255) NULL;
