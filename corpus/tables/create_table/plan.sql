CREATE TABLE public.users (
    id bigint NOT NULL,
    email text NOT NULL,
    name character varying(100),
    created_at timestamp with time zone DEFAULT now() NOT NULL
);
