CREATE TABLE public.t_2 PARTITION OF public.t FOR VALUES FROM (100) TO (200);
COMMENT ON COLUMN public.t_1.v IS 'new';
COMMENT ON COLUMN public.t_2.id IS 'key';
