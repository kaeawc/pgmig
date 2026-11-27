-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE UNIQUE INDEX CONCURRENTLY t_a_key ON public.t USING btree (a) INCLUDE (b, c);
ALTER TABLE public.t ADD CONSTRAINT t_a_key UNIQUE USING INDEX t_a_key;
