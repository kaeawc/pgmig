CREATE SCHEMA app;
CREATE SEQUENCE app.s;
CREATE TYPE app.mood AS ENUM ('a', 'b');
CREATE DOMAIN app.posint AS integer CHECK (VALUE > 0);
CREATE TABLE app.u (id integer NOT NULL, n integer, CONSTRAINT u_pkey PRIMARY KEY (id));
CREATE TABLE t (id int, extra text, CONSTRAINT t_id_key UNIQUE (id));
CREATE INDEX t_id_idx ON t (id);
