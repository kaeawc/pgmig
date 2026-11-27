CREATE DOMAIN public.posint AS integer CHECK (VALUE > 0);
CREATE TYPE public.pair AS (a bigint, b public.posint, c integer);
