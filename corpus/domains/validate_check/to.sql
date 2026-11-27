CREATE DOMAIN public.score AS integer CONSTRAINT score_min CHECK (VALUE >= 0);
CREATE TABLE public.t (s public.score);
