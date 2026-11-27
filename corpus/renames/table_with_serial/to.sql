-- The serial column's sequence is renamed with its table, so its value carries on.
-- pgmig:rename table public.users accounts
CREATE TABLE accounts (id serial PRIMARY KEY, name text);
