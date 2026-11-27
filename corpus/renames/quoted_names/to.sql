-- pgmig:rename table public."Mixed Case" "Other Case"
-- pgmig:rename column public."Other Case"."Old Name" "New Name"
CREATE TABLE "Other Case" ("New Name" text, "other" integer);
