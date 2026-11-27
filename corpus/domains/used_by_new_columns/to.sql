CREATE DOMAIN public.zz_amount AS numeric(12,2) CHECK (VALUE >= 0);
CREATE TABLE public.aa (total public.zz_amount NOT NULL DEFAULT 0, parts public.zz_amount[]);
CREATE TABLE public.zz (id int, amount public.zz_amount);
