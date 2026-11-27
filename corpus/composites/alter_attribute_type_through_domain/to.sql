CREATE TYPE public.pair AS (a bigint);
CREATE DOMAIN public.d AS public.pair;
CREATE TABLE public.t (id integer, new public.d[]);
