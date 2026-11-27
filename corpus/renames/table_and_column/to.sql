-- pgmig:rename table public.users accounts
-- pgmig:rename column public.accounts.name display_name
CREATE TABLE accounts (id integer, display_name text, CONSTRAINT users_pkey PRIMARY KEY (id));
CREATE TABLE posts (id integer PRIMARY KEY, author integer, CONSTRAINT posts_author_fkey FOREIGN KEY (author) REFERENCES accounts (id));
