CREATE TABLE public.n (
    id integer NOT NULL,
    a integer,
    CONSTRAINT n_a_key UNIQUE (a),
    CONSTRAINT n_pkey PRIMARY KEY (id)
);
