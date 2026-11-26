CREATE SCHEMA b;
CREATE TABLE b.pt (x int, y int);
CREATE SCHEMA a;
CREATE TABLE a.route (id int, origin b.pt);
