CREATE INDEX t_a_idx ON public.t USING btree (a) INCLUDE (b, id);
