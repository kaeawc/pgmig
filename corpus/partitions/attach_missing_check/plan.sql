ALTER TABLE public.t_2 ADD CONSTRAINT t_v_check CHECK ((v <> ''::text));
ALTER TABLE public.t ATTACH PARTITION public.t_2 FOR VALUES FROM (1) TO (10);
