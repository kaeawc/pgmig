CREATE TYPE mood AS ENUM ('sad', 'ok');
CREATE TABLE people (id integer, mood mood DEFAULT 'ok', moods mood[]);
