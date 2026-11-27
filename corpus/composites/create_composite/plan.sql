CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad', 'happy');
CREATE DOMAIN app.posint AS integer CONSTRAINT posint_check CHECK ((VALUE > 0));
CREATE TYPE public.empty AS ();
CREATE TYPE public.reading AS (
    at timestamp with time zone,
    value numeric(10,2),
    mood app.mood,
    counts app.posint[]
);
COMMENT ON TYPE public.reading IS 'one reading';
