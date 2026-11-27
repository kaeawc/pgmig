ALTER TYPE public.pair ALTER ATTRIBUTE a TYPE bigint;
CREATE TABLE public.t (
    p public.pair[]
);
