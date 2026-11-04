-- Leagues, managed in the web admin (a new server has none). One row per league the client
-- sees; a family is a base league and its Hardcore / SSF / HC SSF variants (`family` = the base's
-- id). Wire mapping: docs/protocol/login.md, "LeagueEntry". Times are unix seconds.
CREATE TABLE leagues (
    id           TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    family       TEXT NOT NULL,
    hardcore     INTEGER NOT NULL DEFAULT 0,
    ssf          INTEGER NOT NULL DEFAULT 0,
    -- LeagueFlag.Id of the banner (NULL: the client's default art).
    flag         TEXT,
    -- JSON list of CoreLeagues ids; stored only, nothing is implemented.
    mechanics    TEXT NOT NULL DEFAULT '[]',
    permanent    INTEGER NOT NULL DEFAULT 0,
    starts_at    INTEGER,
    ends_at      INTEGER,
    state        TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'hidden', 'ended')),
    -- The family characters move to when this family ends (at ends_at or "End now").
    migrate_to   TEXT,
    is_default   INTEGER NOT NULL DEFAULT 0,
    sort         INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
CREATE INDEX leagues_family ON leagues (family);

-- One line per character moved when a league ended.
CREATE TABLE league_moves (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    character_id INTEGER NOT NULL,
    name         TEXT NOT NULL,
    from_league  TEXT NOT NULL,
    to_league    TEXT NOT NULL,
    at           INTEGER NOT NULL
);
