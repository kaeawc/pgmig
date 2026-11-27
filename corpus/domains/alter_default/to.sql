CREATE DOMAIN public.a AS integer DEFAULT 7;
CREATE DOMAIN public.b AS integer DEFAULT 2;
CREATE DOMAIN public.c AS integer;
CREATE TABLE public.t (a public.a, b public.b, c public.c);
