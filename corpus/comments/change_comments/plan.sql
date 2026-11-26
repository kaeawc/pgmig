CREATE SCHEMA fresh;
CREATE TABLE fresh.made (
    id integer
);
ALTER TABLE keep.items ADD COLUMN added text;
DROP TABLE keep.dropped;
COMMENT ON SCHEMA fresh IS 'a new schema';
COMMENT ON SCHEMA gone_comment IS NULL;
COMMENT ON SCHEMA keep IS 'the owner''s schema';
COMMENT ON TABLE fresh.made IS 'made with a comment';
COMMENT ON COLUMN fresh.made.id IS 'made id';
COMMENT ON TABLE keep.items IS 'the owner''s items';
COMMENT ON COLUMN keep.items.name IS 'changed name comment';
COMMENT ON COLUMN keep.items.note IS NULL;
COMMENT ON COLUMN keep.items.extra IS 'newly commented';
COMMENT ON COLUMN keep.items.added IS 'a column''s comment on a new column';
COMMENT ON TABLE keep.plain IS 'set on a table without one';
COMMENT ON COLUMN keep.plain.id IS 'line one
line two with a back\slash';
