CREATE DOMAIN public.score AS integer
    CONSTRAINT score_max CHECK (VALUE <= 100)
    CONSTRAINT score_odd CHECK (VALUE <> 7);
CREATE TABLE public.t (s public.score);
