CREATE TABLE public.t (
    id integer,
    a integer
);
CREATE INDEX t_a_idx ON public.t USING btree (a);
