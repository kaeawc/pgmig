CREATE DOMAIN public.a AS integer;
CREATE DOMAIN public.b AS integer DEFAULT 1;
CREATE DOMAIN public.c AS integer DEFAULT 1;
CREATE TABLE public.t (a public.a, b public.b, c public.c);
