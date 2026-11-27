-- The column is already NOT NULL: the key is built and attached.
CREATE TABLE t (id int PRIMARY KEY, a int);
-- SET NOT NULL first, committed before the build, so USING INDEX finds the column set.
CREATE TABLE n (id int PRIMARY KEY, a int);
