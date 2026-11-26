CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad', 'it''s complicated', 'happy');
COMMENT ON TYPE app.mood IS 'how it''s going';
CREATE TYPE public.status AS ENUM ('open', 'closed');
CREATE TYPE public.nothing_yet AS ENUM ();
CREATE TABLE app.people (id int, mood app.mood DEFAULT 'sad' NOT NULL, history app.mood[]);
CREATE TABLE public.tickets (id int, status status DEFAULT 'open');
