CREATE TABLE t (a int CONSTRAINT a_pos CHECK (a > 0), b int CONSTRAINT b_key UNIQUE, c int CONSTRAINT c_old CHECK (c > 0));
COMMENT ON CONSTRAINT a_pos ON t IS 'old';
COMMENT ON CONSTRAINT b_key ON t IS 'removed';
COMMENT ON CONSTRAINT c_old ON t IS 'follows the rename';
