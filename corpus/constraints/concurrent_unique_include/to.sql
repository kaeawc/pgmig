CREATE TABLE t (a int, b int, c text, CONSTRAINT t_a_key UNIQUE (a) INCLUDE (b, c));
