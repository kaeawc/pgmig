CREATE TABLE t (a int CONSTRAINT a_pos CHECK (a > 0), b int CONSTRAINT b_key UNIQUE, c int CONSTRAINT c_new CHECK (c > 0));
COMMENT ON CONSTRAINT a_pos ON t IS 'the owner''s';
COMMENT ON CONSTRAINT c_new ON t IS 'follows the rename';
CREATE TABLE "Made" (id int CONSTRAINT "Made_pkey" PRIMARY KEY);
COMMENT ON CONSTRAINT "Made_pkey" ON "Made" IS 'on a new table';
