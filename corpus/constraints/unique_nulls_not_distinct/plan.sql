CREATE TABLE public.made (
    k integer,
    v integer,
    CONSTRAINT made_k_v_key UNIQUE NULLS NOT DISTINCT (k, v)
);
ALTER TABLE public.r DROP CONSTRAINT r_a_fkey;
ALTER TABLE public.u DROP CONSTRAINT u_a_key;
ALTER TABLE public.u ADD CONSTRAINT u_a_key UNIQUE NULLS NOT DISTINCT (a);
ALTER TABLE public.r ADD CONSTRAINT r_a_fkey FOREIGN KEY (a) REFERENCES u(a);
ALTER TABLE public.u ADD CONSTRAINT u_b_key UNIQUE (b);
