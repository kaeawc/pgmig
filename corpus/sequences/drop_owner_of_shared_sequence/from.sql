CREATE TABLE t (id serial, n int);
CREATE TABLE z (x bigint DEFAULT nextval('t_id_seq'));
