CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('sad', 'happy');
CREATE DOMAIN public.z_mood AS app.mood DEFAULT 'sad';
CREATE DOMAIN public.y_mood AS public.z_mood;
CREATE TABLE public.people (mood public.y_mood);
