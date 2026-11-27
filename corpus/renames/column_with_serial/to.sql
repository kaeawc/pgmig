-- The serial column's sequence is renamed with its column, so its value carries on.
-- pgmig:rename column public.users.id user_id
CREATE TABLE users (user_id serial PRIMARY KEY, name text);
