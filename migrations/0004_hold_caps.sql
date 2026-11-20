-- Who placed a hold, so open holds are capped per principal (END_GOAL invariant 9, D-017).
-- Rows from before this migration have none and count against no one.
ALTER TABLE bookings ADD COLUMN held_by uuid;
CREATE INDEX bookings_open_holds ON bookings (business_id, held_by, hold_expires_at)
    WHERE status = 'held';
