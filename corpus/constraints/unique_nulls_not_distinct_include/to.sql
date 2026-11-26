CREATE TABLE u (a int, b int, c text, CONSTRAINT u_a_key UNIQUE NULLS NOT DISTINCT (a) INCLUDE (b, c));
CREATE TABLE made (k int, v text, UNIQUE (k) INCLUDE (v));
