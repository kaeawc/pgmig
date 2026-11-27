ALTER TABLE public.users RENAME TO accounts;
ALTER TABLE public.accounts RENAME COLUMN name TO display_name;
