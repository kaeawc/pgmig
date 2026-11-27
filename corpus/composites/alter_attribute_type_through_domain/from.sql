CREATE TYPE public.pair AS (a integer);
CREATE DOMAIN public.d AS public.pair;
CREATE TABLE public.t (id integer, old public.d);
