CREATE SCHEMA app;
CREATE TABLE app."Users" (id int, "Email" text);
CREATE INDEX "Users_email_idx" ON app."Users" (lower("Email"));
