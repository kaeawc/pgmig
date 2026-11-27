DROP INDEX public.t_old_idx;
CREATE INDEX t_new_idx ON public.t USING btree (a);
