-- A value before the first, one between two, one after the last; the new default uses a new
-- value, so it must run after the ADD VALUE statements are committed.
CREATE TYPE mood AS ENUM ('awful', 'sad', 'ok', 'meh', 'happy', 'ecstatic');
CREATE TABLE people (id int, mood mood DEFAULT 'ok');
