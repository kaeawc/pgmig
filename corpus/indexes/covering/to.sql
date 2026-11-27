CREATE TABLE t (id int, a int, b text);
CREATE INDEX t_a_idx ON t (a) INCLUDE (b, id);
