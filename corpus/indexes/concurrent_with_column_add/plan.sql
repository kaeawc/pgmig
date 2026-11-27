ALTER TABLE public.t ADD COLUMN email text;
-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE INDEX CONCURRENTLY t_email_idx ON public.t USING btree (lower(email));
