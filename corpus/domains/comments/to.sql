CREATE DOMAIN public.score AS integer CONSTRAINT score_min CHECK (VALUE >= 0);
CREATE DOMAIN public.fresh AS integer CONSTRAINT fresh_min CHECK (VALUE >= 0);
COMMENT ON DOMAIN public.score IS 'points';
COMMENT ON CONSTRAINT score_min ON DOMAIN public.score IS 'never negative';
COMMENT ON CONSTRAINT fresh_min ON DOMAIN public.fresh IS 'new';
