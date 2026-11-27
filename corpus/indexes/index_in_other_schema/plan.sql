CREATE SCHEMA app;
CREATE TABLE app."Users" (
    id integer,
    "Email" text
);
CREATE INDEX "Users_email_idx" ON app."Users" USING btree (lower("Email"));
