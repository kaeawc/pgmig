CREATE INDEX t_n_brin_idx ON public.t USING brin (n);
CREATE INDEX t_n_hash_idx ON public.t USING hash (n);
CREATE INDEX t_span_idx ON public.t USING gist (span);
CREATE INDEX t_tags_idx ON public.t USING gin (tags);
