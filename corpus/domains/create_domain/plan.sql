CREATE SCHEMA app;
CREATE DOMAIN app.code AS character varying(20) CONSTRAINT code_check CHECK (((VALUE)::text <> ''::text));
CREATE DOMAIN public."App Name" AS text DEFAULT 'it''s'::text;
CREATE DOMAIN public.posint AS integer NOT NULL DEFAULT 1 CONSTRAINT posint_check CHECK ((VALUE > 0)) CONSTRAINT posint_small CHECK ((VALUE < 1000000));
COMMENT ON TYPE public.posint IS 'a positive integer';
