CREATE TABLE renamed (id int CONSTRAINT renamed_old_pkey PRIMARY KEY);
CREATE TABLE changed (a int, b int, CONSTRAINT changed_pkey PRIMARY KEY (a));
CREATE TABLE dropped_pk (id int PRIMARY KEY);
CREATE TABLE added_pk (id int, note text CONSTRAINT note_short CHECK (length(note) < 10));
