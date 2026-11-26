CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad', 'it''s complicated', 'happy');
CREATE TYPE public.nothing_yet AS ENUM ();
CREATE TYPE public.status AS ENUM ('open', 'closed');
CREATE TABLE app.people (
    id integer,
    mood app.mood DEFAULT 'sad'::app.mood NOT NULL,
    history app.mood[]
);
ALTER TABLE public.tickets ADD COLUMN status public.status DEFAULT 'open'::public.status;
COMMENT ON TYPE app.mood IS 'how it''s going';
