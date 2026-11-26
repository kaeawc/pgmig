CREATE TABLE public.created (
    id integer NOT NULL,
    name text NOT NULL,
    CONSTRAINT created_pkey PRIMARY KEY (id),
    CONSTRAINT name_set CHECK ((name <> ''::text))
);
ALTER TABLE public.added_pk DROP CONSTRAINT note_short;
ALTER TABLE public.changed DROP CONSTRAINT changed_pkey;
ALTER TABLE public.dropped_pk DROP CONSTRAINT dropped_pk_pkey;
ALTER TABLE public.added_pk ALTER COLUMN id SET NOT NULL;
ALTER TABLE public.changed ALTER COLUMN b SET NOT NULL;
ALTER TABLE public.dropped_pk ALTER COLUMN id DROP NOT NULL;
ALTER TABLE public.added_pk ADD CONSTRAINT note_short CHECK ((length(note) < 20));
ALTER TABLE public.added_pk ADD CONSTRAINT added_pk_pkey PRIMARY KEY (id);
ALTER TABLE public.changed ADD CONSTRAINT changed_pkey PRIMARY KEY (a, b);
ALTER TABLE public.renamed RENAME CONSTRAINT renamed_old_pkey TO renamed_pkey;
