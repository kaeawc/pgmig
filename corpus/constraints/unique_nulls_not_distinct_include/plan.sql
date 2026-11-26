CREATE TABLE public.made (
    k integer,
    v text,
    CONSTRAINT made_k_v_key UNIQUE (k) INCLUDE (v)
);
ALTER TABLE public.u ADD CONSTRAINT u_a_key UNIQUE NULLS NOT DISTINCT (a) INCLUDE (b, c);
