CREATE TABLE users (id integer PRIMARY KEY, name text);
CREATE TABLE posts (id integer PRIMARY KEY, author integer REFERENCES users (id));
