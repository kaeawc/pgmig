CREATE TABLE t (id int NOT NULL, v text DEFAULT 'none', CONSTRAINT t_id_positive CHECK (id > 0)) PARTITION BY RANGE (id);
CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100);
-- Kept as a table of its own: detached, then given a comment and without the default it had.
CREATE TABLE t_2 (id int NOT NULL, v text, CONSTRAINT t_id_positive CHECK (id > 0));
COMMENT ON TABLE t_2 IS 'archived';
