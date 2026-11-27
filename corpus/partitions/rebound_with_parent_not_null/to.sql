-- The parent's column becomes NOT NULL while its partition's bound moves: set while the partition
-- is attached, so the partition has it when it is attached again.
CREATE TABLE t (id int NOT NULL, v text NOT NULL) PARTITION BY RANGE (id);
CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (200);
