CREATE TABLE parents (id integer PRIMARY KEY);
CREATE TABLE children (id integer PRIMARY KEY, parent integer, CONSTRAINT children_parent_fkey FOREIGN KEY (parent) REFERENCES parents (id));
