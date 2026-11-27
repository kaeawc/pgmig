-- The hint ran already: it stays in the file and changes nothing.
-- pgmig:rename table public.users accounts
CREATE TABLE accounts (id integer, name text);
