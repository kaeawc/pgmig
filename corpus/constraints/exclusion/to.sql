CREATE TABLE booking (during int4range, cancelled boolean, EXCLUDE USING gist (during WITH &&) WHERE (NOT cancelled));
CREATE TABLE slot (during int4range);
CREATE TABLE made (during int4range, EXCLUDE USING gist (during WITH &&) DEFERRABLE INITIALLY DEFERRED);
