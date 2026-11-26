CREATE TABLE public.made (
    b integer,
    CONSTRAINT b_pos CHECK ((b > 0)) NO INHERIT
);
ALTER TABLE public.t DROP CONSTRAINT a_pos;
ALTER TABLE public.t ADD CONSTRAINT a_pos CHECK ((a > 0)) NO INHERIT;
ALTER TABLE public.t ADD CONSTRAINT a_small CHECK ((a < 100)) NO INHERIT;
