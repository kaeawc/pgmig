CREATE DOMAIN public.req AS text;
CREATE DOMAIN public.opt AS text NOT NULL;
CREATE TABLE public.t (r public.req, o public.opt);
INSERT INTO public.t VALUES ('x', 'y');
