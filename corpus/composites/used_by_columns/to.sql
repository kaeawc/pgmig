CREATE TYPE public.zz_point AS (x integer, y integer);
CREATE TYPE public.zz_segment AS (a public.zz_point, b public.zz_point);
CREATE TABLE public.aa (seg public.zz_segment, pts public.zz_point[]);
CREATE TABLE public.zz (id int, at public.zz_point);
