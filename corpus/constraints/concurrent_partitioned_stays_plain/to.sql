-- USING INDEX and CONCURRENTLY are not allowed on a partitioned table: the plain ADD CONSTRAINT stays.
CREATE TABLE p (id int, a int, CONSTRAINT p_id_key UNIQUE (id)) PARTITION BY RANGE (id);
