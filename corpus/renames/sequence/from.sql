CREATE SEQUENCE order_numbers START 100;
CREATE TABLE orders (id bigint DEFAULT nextval('order_numbers'));
