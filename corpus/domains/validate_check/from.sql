CREATE DOMAIN public.score AS integer;
ALTER DOMAIN public.score ADD CONSTRAINT score_min CHECK (VALUE >= 0) NOT VALID;
CREATE TABLE public.t (s public.score);
INSERT INTO public.t VALUES (1);
