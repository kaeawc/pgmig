CREATE TABLE public.made (
    id integer
);
CREATE INDEX made_id_idx ON public.made USING btree (id);
CREATE INDEX "T_c" ON public.t USING btree (c);
COMMENT ON INDEX public.made_id_idx IS 'on a new table';
COMMENT ON INDEX public."T_c" IS 'on a new index';
COMMENT ON INDEX public.t_a_idx IS 'the owner''s';
COMMENT ON INDEX public.t_b_idx IS NULL;
