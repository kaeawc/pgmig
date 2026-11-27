ALTER SEQUENCE public.users_id_seq RENAME TO users_user_id_seq;
ALTER TABLE public.users RENAME COLUMN id TO user_id;
