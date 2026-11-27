ALTER TABLE public.t DROP COLUMN old;
ALTER TYPE public.pair ALTER ATTRIBUTE a TYPE bigint;
ALTER TABLE public.t ADD COLUMN new public.d[];
