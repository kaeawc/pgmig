-- u_a_key is replaced, so the unchanged foreign key using it is dropped first and added again.
CREATE TABLE u (a int, b int, CONSTRAINT u_a_key UNIQUE NULLS NOT DISTINCT (a), CONSTRAINT u_b_key UNIQUE (b));
CREATE TABLE r (a int CONSTRAINT r_a_fkey REFERENCES u (a));
CREATE TABLE made (k int, v int, UNIQUE NULLS NOT DISTINCT (k, v));
