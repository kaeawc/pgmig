CREATE DOMAIN public.score AS integer CONSTRAINT score_min CHECK (VALUE >= 0);
CREATE DOMAIN public.fresh AS integer CONSTRAINT fresh_min CHECK (VALUE >= 0);
ALTER DOMAIN public.score DROP CONSTRAINT score_min;
ALTER DOMAIN public.score ADD CONSTRAINT score_min CHECK (VALUE >= 0) NOT VALID;
ALTER DOMAIN public.fresh ADD CONSTRAINT fresh_max CHECK (VALUE < 10) NOT VALID;
CREATE TABLE public.t (s public.score);
