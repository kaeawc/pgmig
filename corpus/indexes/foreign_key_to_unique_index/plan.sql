ALTER TABLE public.c DROP CONSTRAINT c_pid_fkey;
DROP INDEX public.p_id_idx;
CREATE UNIQUE INDEX p_id_idx ON public.p USING btree (id) NULLS NOT DISTINCT;
ALTER TABLE public.c ADD CONSTRAINT c_pid_fkey FOREIGN KEY (pid) REFERENCES public.p(id);
