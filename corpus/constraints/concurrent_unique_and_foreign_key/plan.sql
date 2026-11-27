-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE UNIQUE INDEX CONCURRENTLY p_code_key ON public.p USING btree (code);
ALTER TABLE public.p ADD CONSTRAINT p_code_key UNIQUE USING INDEX p_code_key;
ALTER TABLE public.c ADD CONSTRAINT c_p_code_fkey FOREIGN KEY (p_code) REFERENCES public.p(code);
