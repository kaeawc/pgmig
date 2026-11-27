ALTER TABLE public.users RENAME TO accounts;
ALTER SEQUENCE public.users_id_seq RENAME TO accounts_id_seq;
ALTER TABLE public.accounts RENAME CONSTRAINT users_pkey TO accounts_pkey;
