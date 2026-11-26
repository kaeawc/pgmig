CREATE TABLE parent (id int PRIMARY KEY);
CREATE TABLE child (
    id int,
    pid int REFERENCES parent,
    CONSTRAINT child_pid_fkey2 FOREIGN KEY (id) REFERENCES parent (id)
);
CREATE TABLE other (id int PRIMARY KEY);
CREATE TABLE leaf (oid int REFERENCES other);
