CREATE DOMAIN positive AS integer CONSTRAINT is_positive CHECK (VALUE > 0);
CREATE TABLE items (qty positive DEFAULT 1);
