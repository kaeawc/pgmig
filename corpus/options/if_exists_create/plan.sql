CREATE SCHEMA IF NOT EXISTS app;
CREATE TYPE app.mood AS ENUM ('a', 'b');
CREATE DOMAIN app.posint AS integer CONSTRAINT posint_check CHECK ((VALUE > 0));
CREATE SEQUENCE IF NOT EXISTS app.s;
CREATE TABLE IF NOT EXISTS app.u (
    id integer NOT NULL,
    n app.posint,
    CONSTRAINT u_pkey PRIMARY KEY (id)
);
ALTER TABLE public.t ADD COLUMN IF NOT EXISTS extra text;
CREATE INDEX IF NOT EXISTS u_n_idx ON app.u USING btree (n);
CREATE INDEX IF NOT EXISTS t_id_idx ON public.t USING btree (id);
