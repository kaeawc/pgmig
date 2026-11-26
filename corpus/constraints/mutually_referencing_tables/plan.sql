CREATE TABLE public.a (
    id integer NOT NULL,
    b_id integer,
    CONSTRAINT a_pkey PRIMARY KEY (id)
);
CREATE TABLE public.b (
    id integer NOT NULL,
    a_id integer,
    CONSTRAINT b_pkey PRIMARY KEY (id)
);
CREATE TABLE public.tree (
    id integer NOT NULL,
    parent integer,
    CONSTRAINT tree_pkey PRIMARY KEY (id)
);
ALTER TABLE public.a ADD CONSTRAINT a_b_id_fkey FOREIGN KEY (b_id) REFERENCES public.b(id);
ALTER TABLE public.b ADD CONSTRAINT b_a_id_fkey FOREIGN KEY (a_id) REFERENCES public.a(id);
ALTER TABLE public.tree ADD CONSTRAINT tree_parent_fkey FOREIGN KEY (parent) REFERENCES public.tree(id);
