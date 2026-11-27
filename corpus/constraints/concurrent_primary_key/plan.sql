ALTER TABLE public.n ALTER COLUMN id SET NOT NULL;
-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE UNIQUE INDEX CONCURRENTLY n_pkey ON public.n USING btree (id);
ALTER TABLE public.n ADD CONSTRAINT n_pkey PRIMARY KEY USING INDEX n_pkey;
-- Runs outside a transaction: Postgres refuses it inside one.
-- pgmig:notx
CREATE UNIQUE INDEX CONCURRENTLY t_pkey ON public.t USING btree (id);
ALTER TABLE public.t ADD CONSTRAINT t_pkey PRIMARY KEY USING INDEX t_pkey;
