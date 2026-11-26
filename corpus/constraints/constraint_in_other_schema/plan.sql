CREATE SCHEMA billing;
CREATE TABLE billing.lines (
    item integer,
    qty integer,
    CONSTRAINT lines_qty_check CHECK ((qty <> 0))
);
ALTER TABLE app.items DROP CONSTRAINT qty_positive;
ALTER TABLE billing.lines ADD CONSTRAINT lines_item_fkey FOREIGN KEY (item) REFERENCES app.items(id);
ALTER TABLE app.items ADD CONSTRAINT qty_positive CHECK ((qty >= 0));
COMMENT ON CONSTRAINT lines_item_fkey ON billing.lines IS 'an item in another schema';
