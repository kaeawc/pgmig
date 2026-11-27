CREATE TABLE t (id int, a int, b int);
CREATE UNIQUE INDEX t_a_b_idx ON t (a, b);
CREATE UNIQUE INDEX t_abs_id_idx ON t (abs(id));
