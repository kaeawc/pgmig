CREATE TABLE t (id int PRIMARY KEY, a int, b text UNIQUE);
CREATE INDEX t_a_idx ON t (a);
CREATE INDEX t_lower_b_idx ON t (lower(b)) WHERE a > 0;
CREATE TABLE kept (id int);
