DROP TABLE public.old;
DROP DOMAIN public.amount;
CREATE DOMAIN public.amount AS bigint DEFAULT 0 CONSTRAINT amount_check CHECK ((VALUE >= 0));
ALTER TABLE public.keep ADD COLUMN a public.amount;
