-- The leaderboard the c4-tournament package writes. `c4-register` runs this
-- statement itself before its first write (db_write accepts DDL), so the
-- file is here for an organiser who wants the table on another database
-- ahead of time, or to inspect what the workflows expect.
CREATE TABLE IF NOT EXISTS leaderboard (
  model TEXT PRIMARY KEY,
  parameters INTEGER NOT NULL,
  artifact_bytes INTEGER NOT NULL,
  wins INTEGER NOT NULL DEFAULT 0,
  losses INTEGER NOT NULL DEFAULT 0,
  draws INTEGER NOT NULL DEFAULT 0
);
