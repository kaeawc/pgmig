CREATE SCHEMA app;
CREATE TABLE app.items (id int CONSTRAINT items_pkey PRIMARY KEY, qty int CONSTRAINT qty_positive CHECK (qty > 0));
