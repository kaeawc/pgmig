CREATE TABLE t (id int, a int, done boolean);
CREATE INDEX t_a_pos_idx ON t (a) WHERE a > 0;
