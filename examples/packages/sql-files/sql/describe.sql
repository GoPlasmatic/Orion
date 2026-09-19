-- Describe a word: its upper-case form and its length.
--
-- The statement lives here rather than as one JSON string, so it can be
-- read, commented and reviewed. `compile` inlines it in normal form:
-- comments and whitespace collapse, strings stay byte-exact.
SELECT upper($1)  AS shout,   -- SQLite's upper()
       length($1) AS letters
