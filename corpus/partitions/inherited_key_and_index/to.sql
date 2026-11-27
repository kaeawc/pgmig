CREATE TABLE t (id int NOT NULL, v text, CONSTRAINT t_pkey PRIMARY KEY (id), CONSTRAINT t_v_check CHECK (v <> '')) PARTITION BY RANGE (id);
CREATE INDEX t_v_idx ON t (v);
-- The partitions have the key, check and index from the parent: the plan does not repeat them.
CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100);
CREATE TABLE t_2 PARTITION OF t FOR VALUES FROM (100) TO (200);
-- An index of a partition's own.
CREATE INDEX t_2_id_v_idx ON t_2 (id, v);
