-- pgmig:rename sequence public.order_numbers order_ids
CREATE SEQUENCE order_ids START 100;
CREATE TABLE orders (id bigint DEFAULT nextval('order_ids'));
