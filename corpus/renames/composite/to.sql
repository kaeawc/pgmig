-- pgmig:rename type public.pair couple
CREATE TYPE couple AS (a integer, b text);
CREATE TABLE things (p couple);
