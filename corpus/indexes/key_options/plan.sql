CREATE INDEX t_a_b_idx ON public.t USING btree (a DESC NULLS LAST, b text_pattern_ops);
CREATE INDEX t_a_nulls_first_idx ON public.t USING btree (a NULLS FIRST);
CREATE INDEX t_c_idx ON public.t USING btree (c COLLATE "C" DESC);
