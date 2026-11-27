CREATE TABLE public.sales (
    id integer NOT NULL,
    region text NOT NULL,
    sold date NOT NULL
) PARTITION BY LIST (region);
CREATE TABLE public.sales_east PARTITION OF public.sales FOR VALUES IN ('east');
CREATE TABLE public.sales_west PARTITION OF public.sales FOR VALUES IN ('west') PARTITION BY RANGE (sold);
CREATE TABLE public.sales_west_2026 PARTITION OF public.sales_west FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
