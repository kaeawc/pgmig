-- renames a value: rows holding 'ok' will hold 'fine'
ALTER TYPE public.mood RENAME VALUE 'ok' TO 'fine';
ALTER TABLE public.people ALTER COLUMN mood SET DEFAULT 'fine'::public.mood;
COMMENT ON TYPE public.mood IS 'new comment';
