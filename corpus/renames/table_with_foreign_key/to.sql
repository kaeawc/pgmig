-- pgmig:rename table public.parents guardians
CREATE TABLE guardians (id integer, CONSTRAINT parents_pkey PRIMARY KEY (id));
CREATE TABLE children (id integer PRIMARY KEY, parent integer, CONSTRAINT children_parent_fkey FOREIGN KEY (parent) REFERENCES guardians (id));
