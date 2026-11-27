CREATE SCHEMA app;
CREATE SEQUENCE app.s;
CREATE TYPE app.mood AS ENUM ('a', 'b');
CREATE DOMAIN app.posint AS integer CHECK (VALUE > 0);
CREATE TABLE app.u (id integer NOT NULL, n app.posint, CONSTRAINT u_pkey PRIMARY KEY (id));
CREATE INDEX u_n_idx ON app.u (n);
CREATE TABLE t (id int, extra text);
CREATE INDEX t_id_idx ON t (id);
