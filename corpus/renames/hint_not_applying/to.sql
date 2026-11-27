-- The hint names a table that does not exist: ignored, so this is a drop and a create.
-- pgmig:rename table public.people accounts
CREATE TABLE accounts (id integer, name text);
