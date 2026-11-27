CREATE TABLE t (n int);
CREATE INDEX t_n_idx ON t (n) WITH (fillfactor = 70);
