CREATE SCHEMA app;
CREATE SCHEMA billing;
CREATE TABLE app.items (id int CONSTRAINT items_pkey PRIMARY KEY, qty int CONSTRAINT qty_positive CHECK (qty >= 0));
CREATE TABLE billing.lines (item int CONSTRAINT lines_item_fkey REFERENCES app.items, qty int CHECK (qty <> 0));
COMMENT ON CONSTRAINT lines_item_fkey ON billing.lines IS 'an item in another schema';
