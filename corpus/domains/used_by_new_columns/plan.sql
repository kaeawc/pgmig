CREATE DOMAIN public.zz_amount AS numeric(12,2) CONSTRAINT zz_amount_check CHECK ((VALUE >= (0)::numeric));
CREATE TABLE public.aa (
    total public.zz_amount DEFAULT 0 NOT NULL,
    parts public.zz_amount[]
);
ALTER TABLE public.zz ADD COLUMN amount public.zz_amount;
