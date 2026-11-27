CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL);
CREATE INDEX users_name_idx ON users (name);
ALTER TABLE users ADD CONSTRAINT users_name_check CHECK (length(name) > 0);
COMMENT ON COLUMN users.name IS 'what people call it';
