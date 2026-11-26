CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
COMMENT ON TYPE mood IS 'old comment';
CREATE TABLE people (id int, mood mood DEFAULT 'ok', moods mood[]);
