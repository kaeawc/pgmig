DROP INDEX public.t_a_idx;
CREATE UNIQUE INDEX t_a_idx ON public.t USING btree (a) NULLS NOT DISTINCT;
CREATE UNIQUE INDEX t_b_idx ON public.t USING btree (b) NULLS NOT DISTINCT WHERE (b > 0);
