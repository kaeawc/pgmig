CREATE DOMAIN public.name AS text COLLATE pg_catalog."C" CONSTRAINT name_check CHECK ((VALUE <> ''::text));
DROP DOMAIN public.code;
CREATE DOMAIN public.code AS text COLLATE pg_catalog."C";
