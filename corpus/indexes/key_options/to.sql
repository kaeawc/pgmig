CREATE TABLE t (a int, b text, c text);
CREATE INDEX t_a_b_idx ON t (a DESC NULLS LAST, b text_pattern_ops);
CREATE INDEX t_a_nulls_first_idx ON t (a NULLS FIRST);
CREATE INDEX t_c_idx ON t (c COLLATE "C" DESC);
