CREATE DOMAIN public.amount AS integer DEFAULT 0 CHECK (VALUE >= 0);
CREATE TABLE public.old (a public.amount);
CREATE TABLE public.keep (id int);
