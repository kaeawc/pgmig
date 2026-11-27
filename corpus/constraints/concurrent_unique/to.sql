-- Built CONCURRENTLY under the constraint's name, then attached with USING INDEX.
CREATE TABLE t (id int, a int, CONSTRAINT t_a_key UNIQUE (a));
