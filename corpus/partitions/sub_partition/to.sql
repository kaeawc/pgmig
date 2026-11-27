CREATE TABLE sales (id int NOT NULL, region text NOT NULL, sold date NOT NULL) PARTITION BY LIST (region);
-- A partition that is partitioned in turn.
CREATE TABLE sales_west PARTITION OF sales FOR VALUES IN ('west') PARTITION BY RANGE (sold);
CREATE TABLE sales_west_2026 PARTITION OF sales_west FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE sales_east PARTITION OF sales FOR VALUES IN ('east');
