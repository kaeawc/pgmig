-- The referenced and the referencing columns change type together: each foreign key is dropped
-- before the type changes and added again after.
CREATE TABLE parent (id text PRIMARY KEY);
CREATE TABLE child (
    id text,
    pid text REFERENCES parent,
    CONSTRAINT child_pid_fkey2 FOREIGN KEY (id) REFERENCES parent (id)
);
-- Only the referencing column changes.
CREATE TABLE other (id int PRIMARY KEY);
CREATE TABLE leaf (oid bigint REFERENCES other);
