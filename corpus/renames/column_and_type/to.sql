-- Renamed, and wider: a RENAME COLUMN, then the type change.
-- pgmig:rename column public.players.score points
CREATE TABLE players (id integer PRIMARY KEY, points bigint NOT NULL DEFAULT 0);
