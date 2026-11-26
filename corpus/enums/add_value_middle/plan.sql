ALTER TYPE public.mood ADD VALUE 'awful' BEFORE 'sad';
ALTER TYPE public.mood ADD VALUE 'ok' AFTER 'sad';
ALTER TYPE public.mood ADD VALUE 'meh' AFTER 'ok';
ALTER TYPE public.mood ADD VALUE 'ecstatic' AFTER 'happy';
-- New enum values cannot be used in the transaction that adds them.
-- pgmig:commit
ALTER TABLE public.people ALTER COLUMN mood SET DEFAULT 'ok'::public.mood;
