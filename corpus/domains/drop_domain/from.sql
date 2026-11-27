CREATE DOMAIN public.posint AS integer CHECK (VALUE > 0);
CREATE DOMAIN public.unused AS text;
CREATE TABLE public.counts (n public.posint, ns public.posint[]);
CREATE TABLE public.keep (id int);
