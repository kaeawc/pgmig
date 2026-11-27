CREATE TABLE public.measurements (
    id integer NOT NULL,
    taken date NOT NULL,
    value numeric DEFAULT 0
) PARTITION BY RANGE (taken);
CREATE TABLE public.measurements_2025 PARTITION OF public.measurements FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE public.measurements_2026 PARTITION OF public.measurements FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE public.measurements_other PARTITION OF public.measurements DEFAULT;
