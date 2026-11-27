-- Renaming the index keeps it (no rebuild).
-- pgmig:rename index public.users_email_idx users_by_email
CREATE TABLE users (id integer, email text);
CREATE INDEX users_by_email ON users (email);
