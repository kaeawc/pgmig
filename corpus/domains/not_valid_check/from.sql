CREATE DOMAIN public.score AS integer;
CREATE TABLE public.t (s public.score);
INSERT INTO public.t VALUES (-1);
