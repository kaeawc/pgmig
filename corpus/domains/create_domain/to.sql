CREATE SCHEMA app;
CREATE DOMAIN public.posint AS integer NOT NULL DEFAULT 1
    CONSTRAINT posint_check CHECK (VALUE > 0)
    CONSTRAINT posint_small CHECK (VALUE < 1000000);
CREATE DOMAIN app.code AS varchar(20) CHECK (VALUE <> '');
CREATE DOMAIN "App Name" AS text DEFAULT 'it''s';
COMMENT ON DOMAIN public.posint IS 'a positive integer';
