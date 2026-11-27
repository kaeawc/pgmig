CREATE DOMAIN public.code AS text COLLATE "C";
CREATE DOMAIN public.name AS text COLLATE "C" CHECK (VALUE <> '');
