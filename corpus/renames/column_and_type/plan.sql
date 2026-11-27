ALTER TABLE public.players RENAME COLUMN score TO points;
ALTER TABLE public.players ALTER COLUMN points TYPE bigint;
