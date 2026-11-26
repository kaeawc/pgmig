CREATE TABLE a (id serial);
CREATE TABLE z (x bigint DEFAULT nextval('a_id_seq'));
