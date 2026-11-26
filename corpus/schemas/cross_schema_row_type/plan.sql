CREATE SCHEMA a;
CREATE SCHEMA b;
CREATE TABLE b.pt (
    x integer,
    y integer
);
CREATE TABLE a.route (
    id integer,
    origin b.pt
);
