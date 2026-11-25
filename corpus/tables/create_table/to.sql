CREATE TABLE users (
    id bigint NOT NULL,
    email text NOT NULL,
    name varchar(100),
    created_at timestamptz NOT NULL DEFAULT now()
);
