CREATE TYPE mood AS ENUM ('sad', 'happy');
CREATE TABLE people (id int, mood mood DEFAULT 'sad');
