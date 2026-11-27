CREATE DOMAIN public.fresh AS integer CONSTRAINT fresh_min CHECK ((VALUE >= 0));
COMMENT ON CONSTRAINT fresh_min ON DOMAIN public.fresh IS 'new';
COMMENT ON CONSTRAINT score_min ON DOMAIN public.score IS 'never negative';
COMMENT ON TYPE public.score IS 'points';
