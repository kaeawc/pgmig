CREATE TABLE t (id int NOT NULL, v text, CONSTRAINT t_v_check CHECK (v <> '')) PARTITION BY RANGE (id);
-- Without the parent's check, which ATTACH PARTITION requires.
CREATE TABLE t_2 (id int NOT NULL, v text);
