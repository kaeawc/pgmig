ALTER TABLE public.t ADD COLUMN email text;
CREATE UNIQUE INDEX t_email_idx ON public.t USING btree (lower(email));
