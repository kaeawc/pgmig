-- Same values but one, at the same position: a rename.
CREATE TYPE mood AS ENUM ('sad', 'fine', 'happy');
COMMENT ON TYPE mood IS 'new comment';
CREATE TABLE people (id int, mood mood DEFAULT 'fine', moods mood[]);
