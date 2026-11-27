CREATE TABLE public.events (
    id bigint NOT NULL,
    payload text
) PARTITION BY HASH (id);
CREATE TABLE public.events_0 PARTITION OF public.events FOR VALUES WITH (modulus 2, remainder 0);
CREATE TABLE public.events_1 PARTITION OF public.events FOR VALUES WITH (modulus 2, remainder 1);
