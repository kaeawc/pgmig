-- The parent gains a column while its partition's bound moves: the column is added while the
-- partition is attached (it reaches the partition), then the partition is detached and attached
-- again.
CREATE TABLE t (id int NOT NULL, v text, w int) PARTITION BY RANGE (id);
CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (200);
