CREATE TABLE t (id int NOT NULL, v text, CONSTRAINT t_pkey PRIMARY KEY (id)) PARTITION BY RANGE (id);
CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100);
-- A table loaded on its own, to become a partition: its key is the one the parent gives it, its
-- default and extra column go.
CREATE TABLE t_2 (id int, v text DEFAULT 'x', loaded_at date, CONSTRAINT t_2_pkey PRIMARY KEY (id));
