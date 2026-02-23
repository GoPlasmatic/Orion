-- Add composite index for the most common rule query pattern (list by channel + status)
CREATE INDEX IF NOT EXISTS idx_rules_channel_status ON rules(channel, status);
