CREATE TABLE t (a int, b int, c int);
CREATE INDEX t_a_idx ON t (a);
COMMENT ON INDEX t_a_idx IS 'old';
CREATE INDEX t_b_idx ON t (b);
COMMENT ON INDEX t_b_idx IS 'removed';
