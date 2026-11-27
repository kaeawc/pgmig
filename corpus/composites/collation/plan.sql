CREATE TYPE public.fresh AS (
    s text COLLATE pg_catalog."C"
);
ALTER TYPE public.named ADD ATTRIBUTE m text COLLATE pg_catalog."C";
ALTER TYPE public.named ALTER ATTRIBUTE n TYPE text COLLATE pg_catalog."C";
