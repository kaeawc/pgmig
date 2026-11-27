DROP INDEX public.t_a_b_idx;
CREATE INDEX t_a_b_idx ON public.t USING btree (a, b);
CREATE INDEX t_c_a_idx ON public.t USING btree (c, a);
