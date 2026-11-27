CREATE UNIQUE INDEX t_a_b_idx ON public.t USING btree (a, b);
CREATE UNIQUE INDEX t_abs_id_idx ON public.t USING btree (abs(id));
