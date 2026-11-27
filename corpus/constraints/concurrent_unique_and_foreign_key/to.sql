-- The key is built, then attached, then the foreign key that uses it is added.
CREATE TABLE p (id int, code text, CONSTRAINT p_code_key UNIQUE (code));
CREATE TABLE c (id int, p_code text CONSTRAINT c_p_code_fkey REFERENCES p (code));
