CREATE TABLE t (id serial, n int);
CREATE TABLE ident (id int GENERATED ALWAYS AS IDENTITY);
CREATE SEQUENCE free;
CREATE TABLE keep (x bigint DEFAULT nextval('free'));
CREATE TABLE also_gone (x bigint DEFAULT nextval('free'));
