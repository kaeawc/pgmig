CREATE DOMAIN public.score AS integer
    CONSTRAINT score_min CHECK (VALUE >= 0)
    CONSTRAINT score_max CHECK (VALUE <= 10);
CREATE TABLE public.t (s public.score);
INSERT INTO public.t VALUES (5);
