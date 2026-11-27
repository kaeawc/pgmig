CREATE TABLE public.t (
    id integer NOT NULL,
    v text,
    CONSTRAINT t_pkey PRIMARY KEY (id),
    CONSTRAINT t_v_check CHECK ((v <> ''::text))
) PARTITION BY RANGE (id);
CREATE TABLE public.t_1 PARTITION OF public.t FOR VALUES FROM (1) TO (100);
CREATE TABLE public.t_2 PARTITION OF public.t FOR VALUES FROM (100) TO (200);
CREATE INDEX t_v_idx ON public.t USING btree (v);
CREATE INDEX t_2_id_v_idx ON public.t_2 USING btree (id, v);
