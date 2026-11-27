-- pgmig:rename column public.users.name full_name
CREATE TABLE users (id integer PRIMARY KEY, full_name text NOT NULL);
CREATE INDEX users_name_idx ON users (full_name);
ALTER TABLE users ADD CONSTRAINT users_name_check CHECK (length(full_name) > 0);
COMMENT ON COLUMN users.full_name IS 'what people call it';
