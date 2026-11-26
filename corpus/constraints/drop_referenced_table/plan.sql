ALTER TABLE public.m1 DROP CONSTRAINT m1_m2_fkey;
ALTER TABLE public.m2 DROP CONSTRAINT m2_m1_fkey;
ALTER TABLE public.kept DROP CONSTRAINT kept_pid_fkey;
DROP TABLE public.gone_child;
DROP TABLE public.m1;
DROP TABLE public.m2;
DROP TABLE public.parent;
DROP TABLE public.tree;
