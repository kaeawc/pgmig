CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad', 'happy');
CREATE DOMAIN public.a_mood AS app.mood DEFAULT 'sad' CHECK (VALUE <> 'happy');
CREATE DOMAIN public.b_mood AS public.a_mood;
CREATE TABLE public.people (mood public.b_mood, history public.a_mood[]);
