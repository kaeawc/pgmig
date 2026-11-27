DROP INDEX public.t_a_pos_idx;
CREATE INDEX t_a_pos_idx ON public.t USING btree (a) WHERE (a > 1);
CREATE INDEX t_open_idx ON public.t USING btree (id) WHERE (NOT done);
