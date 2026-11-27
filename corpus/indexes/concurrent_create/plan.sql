-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a);
