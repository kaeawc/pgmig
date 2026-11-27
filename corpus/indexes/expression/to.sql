CREATE TABLE t (id int, email text, name text);
CREATE INDEX t_lower_email_idx ON t (lower(email));
CREATE INDEX t_name_email_idx ON t ((name || email), id);
