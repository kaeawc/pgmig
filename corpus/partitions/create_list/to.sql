CREATE TABLE orders (id int NOT NULL, region text NOT NULL, total int) PARTITION BY LIST (region);
CREATE TABLE orders_west PARTITION OF orders FOR VALUES IN ('ca', 'or', 'wa');
-- A partition with a check of its own.
CREATE TABLE orders_east PARTITION OF orders (CONSTRAINT orders_east_total CHECK (total > 0)) FOR VALUES IN ('ny');
