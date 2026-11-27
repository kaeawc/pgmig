CREATE TYPE public.pair AS (a integer, b integer);
CREATE TYPE public.other AS (x integer);
COMMENT ON COLUMN public.pair.a IS 'first';
COMMENT ON COLUMN public.other.x IS 'only';
