CREATE TABLE parent (id int PRIMARY KEY);
CREATE TABLE kept (id int, pid int CONSTRAINT kept_pid_fkey REFERENCES parent);
CREATE TABLE gone_child (pid int REFERENCES parent);
CREATE TABLE m1 (id int PRIMARY KEY, m2 int);
CREATE TABLE m2 (id int PRIMARY KEY, m1 int REFERENCES m1);
ALTER TABLE m1 ADD FOREIGN KEY (m2) REFERENCES m2;
CREATE TABLE tree (id int PRIMARY KEY, parent int REFERENCES tree);
