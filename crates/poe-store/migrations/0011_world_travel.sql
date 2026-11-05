-- What a character unlocked in the world (P9): waypoints by WorldAreas.Id, the checkpoints it touched
-- per layout, and where it respawns.
CREATE TABLE character_waypoints (
    character_id INTEGER NOT NULL,
    area TEXT NOT NULL,
    PRIMARY KEY (character_id, area)
);
CREATE TABLE character_checkpoints (
    character_id INTEGER NOT NULL,
    area TEXT NOT NULL,
    seed INTEGER NOT NULL,
    x INTEGER NOT NULL,
    y INTEGER NOT NULL,
    PRIMARY KEY (character_id, area, seed, x, y)
);
CREATE TABLE character_respawn (
    character_id INTEGER PRIMARY KEY,
    area TEXT NOT NULL,
    seed INTEGER NOT NULL,
    x INTEGER NOT NULL,
    y INTEGER NOT NULL
);
