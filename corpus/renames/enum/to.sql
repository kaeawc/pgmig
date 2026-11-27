-- pgmig:rename enum public.mood feeling
CREATE TYPE feeling AS ENUM ('sad', 'ok');
CREATE TABLE people (id integer, mood feeling DEFAULT 'ok', moods feeling[]);
