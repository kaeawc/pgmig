-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
DROP INDEX CONCURRENTLY public.t_a_idx;
