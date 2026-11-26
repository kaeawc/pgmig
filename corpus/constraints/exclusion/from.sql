CREATE TABLE booking (during int4range, cancelled boolean);
CREATE TABLE slot (during int4range, CONSTRAINT slot_during_excl EXCLUDE USING gist (during WITH &&));
