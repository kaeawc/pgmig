CREATE TABLE t (id int, a int);
CREATE INDEX t_a_idx ON t (a) WHERE a > 0;
COMMENT ON INDEX t_a_idx IS 'positive values';
