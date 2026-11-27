-- pgmig:rename domain public.positive whole_number
CREATE DOMAIN whole_number AS integer CONSTRAINT is_positive CHECK (VALUE > 0);
CREATE TABLE items (qty whole_number DEFAULT 1);
