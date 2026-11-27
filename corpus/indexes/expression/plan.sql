CREATE INDEX t_lower_email_idx ON public.t USING btree (lower(email));
CREATE INDEX t_name_email_idx ON public.t USING btree (((name || email)), id);
