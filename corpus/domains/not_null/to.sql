CREATE DOMAIN public.req AS text NOT NULL;
CREATE DOMAIN public.opt AS text;
CREATE TABLE public.t (r public.req, o public.opt);
