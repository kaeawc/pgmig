CREATE TABLE t (id int, email text);
CREATE INDEX t_email_idx ON t (lower(email));
