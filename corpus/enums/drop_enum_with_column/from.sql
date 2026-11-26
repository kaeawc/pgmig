CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad', 'happy');
CREATE TYPE app.gone AS ENUM ('x');
COMMENT ON TYPE app.gone IS 'dropped with its type';
CREATE TABLE app.people (id int, mood app.mood, moods app.mood[]);
CREATE TABLE public.keep (id int, g app.gone, gs app.gone[]);
