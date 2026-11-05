-- League migrations: what a hardcore death does (stored now, used when death exists), and who
-- moved a character and why. Wire and rules: docs/guides/server-admin.md, "Leagues".
ALTER TABLE leagues ADD COLUMN hc_death TEXT NOT NULL DEFAULT 'softcore'
    CHECK (hc_death IN ('softcore', 'stay'));
-- 'end' (league ended), 'ssf' (left SSF), 'hardcore' (left hardcore) or 'admin'.
ALTER TABLE league_moves ADD COLUMN kind TEXT NOT NULL DEFAULT 'end';
-- Who did it: an admin's name, `auto`, or the player's account name; empty for old rows.
ALTER TABLE league_moves ADD COLUMN actor TEXT NOT NULL DEFAULT '';
