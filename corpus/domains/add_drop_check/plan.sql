ALTER DOMAIN public.score DROP CONSTRAINT score_max;
ALTER DOMAIN public.score DROP CONSTRAINT score_min;
ALTER DOMAIN public.score ADD CONSTRAINT score_max CHECK ((VALUE <= 100));
ALTER DOMAIN public.score ADD CONSTRAINT score_odd CHECK ((VALUE <> 7));
