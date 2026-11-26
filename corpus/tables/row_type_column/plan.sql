CREATE TABLE public.zpoint (
    x integer NOT NULL,
    y integer NOT NULL
);
CREATE TABLE public.apath (
    id integer NOT NULL,
    start zpoint
);
