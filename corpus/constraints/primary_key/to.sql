-- Same definition, new name: renamed in place.
CREATE TABLE renamed (id int CONSTRAINT renamed_pkey PRIMARY KEY);
-- A new definition: dropped and added again.
CREATE TABLE changed (a int, b int, CONSTRAINT changed_pkey PRIMARY KEY (a, b));
-- The key goes before its column drops NOT NULL.
CREATE TABLE dropped_pk (id int);
CREATE TABLE added_pk (id int PRIMARY KEY, note text CONSTRAINT note_short CHECK (length(note) < 20));
CREATE TABLE created (id int PRIMARY KEY, name text NOT NULL, CONSTRAINT name_set CHECK (name <> ''));
