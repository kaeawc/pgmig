-- The table is called accounts now; its data stays.
-- pgmig:rename table public.users accounts
CREATE TABLE accounts (id integer NOT NULL, name text);
CREATE INDEX users_name_idx ON accounts (name);
