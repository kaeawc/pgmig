CREATE DOMAIN public.amount AS bigint DEFAULT 0 CHECK (VALUE >= 0);
CREATE TABLE public.keep (id int, a public.amount);
