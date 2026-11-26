CREATE TABLE t (a int, b int, c int);
CREATE INDEX t_a_idx ON t (a);
COMMENT ON INDEX t_a_idx IS 'the owner''s';
CREATE INDEX t_b_idx ON t (b);
CREATE INDEX "T_c" ON t (c);
COMMENT ON INDEX "T_c" IS 'on a new index';
CREATE TABLE made (id int);
CREATE INDEX made_id_idx ON made (id);
COMMENT ON INDEX made_id_idx IS 'on a new table';
