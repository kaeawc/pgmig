CREATE TABLE t (id int, a int, b text, c int);
CREATE INDEX t_a_idx ON t (a);
CREATE INDEX t_id_a_idx ON t (id) INCLUDE (a);
CREATE INDEX t_lower_b_idx ON t (lower(b));
CREATE INDEX t_c_idx ON t (c) WHERE b IS NOT NULL;
CREATE INDEX t_id_idx ON t (id);
