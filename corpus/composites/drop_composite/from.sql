CREATE TYPE public.point2 AS (x integer, y integer);
CREATE TYPE public.unused AS (a text);
CREATE TABLE public.shapes (id int, origin public.point2, path public.point2[]);
