-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
DROP INDEX CONCURRENTLY public.t_a_idx;
-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE INDEX CONCURRENTLY t_a_idx ON public.t USING btree (a) WHERE (a > 0);
COMMENT ON INDEX public.t_a_idx IS 'positive values';
