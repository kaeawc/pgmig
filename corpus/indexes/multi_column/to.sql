CREATE TABLE t (a int, b text, c int);
CREATE INDEX t_a_b_idx ON t (a, b);
CREATE INDEX t_c_a_idx ON t (c, a);
