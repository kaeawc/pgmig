-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
DROP INDEX CONCURRENTLY IF EXISTS public.t_a_idx;
-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS t_b_idx ON public.t USING btree (b);
