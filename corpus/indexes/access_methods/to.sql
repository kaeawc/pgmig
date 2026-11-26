CREATE TABLE t (n int, tags int[], span int4range);
CREATE INDEX t_n_hash_idx ON t USING hash (n);
CREATE INDEX t_n_brin_idx ON t USING brin (n);
CREATE INDEX t_tags_idx ON t USING gin (tags);
CREATE INDEX t_span_idx ON t USING gist (span);
