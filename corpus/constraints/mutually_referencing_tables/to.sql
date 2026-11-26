-- Each table references the other, and one references itself: the foreign keys are added once
-- both tables exist.
CREATE TABLE a (id int PRIMARY KEY, b_id int);
CREATE TABLE b (id int PRIMARY KEY, a_id int REFERENCES a);
ALTER TABLE a ADD CONSTRAINT a_b_id_fkey FOREIGN KEY (b_id) REFERENCES b;
CREATE TABLE tree (id int PRIMARY KEY, parent int REFERENCES tree);
