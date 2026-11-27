CREATE TABLE p (id int);
CREATE UNIQUE INDEX p_id_idx ON p (id);
CREATE TABLE c (pid int REFERENCES p (id));
