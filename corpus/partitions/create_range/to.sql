CREATE TABLE measurements (id int NOT NULL, taken date NOT NULL, value numeric DEFAULT 0) PARTITION BY RANGE (taken);
CREATE TABLE measurements_2025 PARTITION OF measurements FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE measurements_2026 PARTITION OF measurements FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE measurements_other PARTITION OF measurements DEFAULT;
