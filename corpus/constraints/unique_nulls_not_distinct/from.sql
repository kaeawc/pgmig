CREATE TABLE u (a int, b int, CONSTRAINT u_a_key UNIQUE (a));
CREATE TABLE r (a int CONSTRAINT r_a_fkey REFERENCES u (a));
