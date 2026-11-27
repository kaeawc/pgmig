CREATE DOMAIN public.fresh AS integer CONSTRAINT fresh_min CHECK ((VALUE >= 0));
ALTER DOMAIN public.fresh ADD CONSTRAINT fresh_max CHECK ((VALUE < 10)) NOT VALID;
ALTER DOMAIN public.score ADD CONSTRAINT score_min CHECK ((VALUE >= 0)) NOT VALID;
