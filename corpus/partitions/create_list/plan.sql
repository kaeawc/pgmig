CREATE TABLE public.orders (
    id integer NOT NULL,
    region text NOT NULL,
    total integer
) PARTITION BY LIST (region);
CREATE TABLE public.orders_east PARTITION OF public.orders (
    CONSTRAINT orders_east_total CHECK ((total > 0))
) FOR VALUES IN ('ny');
CREATE TABLE public.orders_west PARTITION OF public.orders FOR VALUES IN ('ca', 'or', 'wa');
