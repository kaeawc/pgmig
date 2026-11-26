CREATE TABLE public.made (
    during int4range,
    CONSTRAINT made_during_excl EXCLUDE USING gist (during WITH &&) DEFERRABLE INITIALLY DEFERRED
);
ALTER TABLE public.slot DROP CONSTRAINT slot_during_excl;
ALTER TABLE public.booking ADD CONSTRAINT booking_during_excl EXCLUDE USING gist (during WITH &&) WHERE ((NOT cancelled));
