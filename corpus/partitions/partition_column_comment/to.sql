-- A comment on a partition's column is the partition's own: changed on one partition, set on a
-- new one.
CREATE TABLE t (id int NOT NULL, v text) PARTITION BY RANGE (id);
CREATE TABLE t_1 PARTITION OF t FOR VALUES FROM (1) TO (100);
COMMENT ON COLUMN t_1.v IS 'new';
CREATE TABLE t_2 PARTITION OF t FOR VALUES FROM (100) TO (200);
COMMENT ON COLUMN t_2.id IS 'key';
