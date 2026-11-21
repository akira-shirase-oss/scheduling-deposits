-- Scheduling: the business behind a workspace, its services, staff, hours and exceptions,
-- and bookings with the double-booking guarantee (D-004 … D-012).
--
-- Every product table is keyed by (business_id, id), and every reference between them is a
-- composite foreign key that carries business_id: a row can only ever point at a row of the
-- same business, whatever id a caller sends.

-- btree_gist gives uuid an equality operator class for GiST, so one exclusion constraint can
-- say "same staff member AND overlapping range". It is a trusted extension: the database
-- owner may create it. Its own schema keeps its functions out of public, where the
-- protections manifest pins every function.
CREATE SCHEMA extensions;
REVOKE ALL ON SCHEMA extensions FROM PUBLIC;
CREATE EXTENSION btree_gist WITH SCHEMA extensions;

CREATE TABLE businesses (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    -- An IANA name, checked against the zone database by the server on every write and at
    -- startup (it refuses to serve a zone it cannot resolve).
    time_zone text NOT NULL CHECK (time_zone ~ '^[A-Za-z0-9_+/-]{1,64}$'),
    currency text NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    slot_step_minutes smallint NOT NULL CHECK (slot_step_minutes IN (5, 10, 15, 20, 30, 60)),
    min_notice_minutes integer NOT NULL CHECK (min_notice_minutes BETWEEN 0 AND 43200),
    max_advance_days integer NOT NULL CHECK (max_advance_days BETWEEN 1 AND 365),
    hold_minutes smallint NOT NULL CHECK (hold_minutes BETWEEN 1 AND 60),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE services (
    business_id uuid NOT NULL REFERENCES businesses (workspace_id) ON DELETE CASCADE,
    id uuid NOT NULL,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    duration_minutes smallint NOT NULL
        CHECK (duration_minutes BETWEEN 5 AND 720 AND duration_minutes % 5 = 0),
    buffer_before_minutes smallint NOT NULL
        CHECK (buffer_before_minutes BETWEEN 0 AND 240 AND buffer_before_minutes % 5 = 0),
    buffer_after_minutes smallint NOT NULL
        CHECK (buffer_after_minutes BETWEEN 0 AND 240 AND buffer_after_minutes % 5 = 0),
    -- Integer minor units of the business's currency.
    price_minor bigint NOT NULL CHECK (price_minor BETWEEN 0 AND 100000000),
    deposit_kind text NOT NULL CHECK (deposit_kind IN ('none', 'fixed', 'percent')),
    deposit_value bigint NOT NULL,
    active boolean NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (business_id, id),
    CHECK (CASE deposit_kind
               WHEN 'none' THEN deposit_value = 0
               WHEN 'fixed' THEN deposit_value BETWEEN 1 AND price_minor
               WHEN 'percent' THEN deposit_value BETWEEN 1 AND 100
           END)
);

CREATE TABLE staff (
    business_id uuid NOT NULL REFERENCES businesses (workspace_id) ON DELETE CASCADE,
    id uuid NOT NULL,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    active boolean NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (business_id, id)
);

CREATE TABLE staff_services (
    business_id uuid NOT NULL,
    staff_id uuid NOT NULL,
    service_id uuid NOT NULL,
    PRIMARY KEY (business_id, staff_id, service_id),
    FOREIGN KEY (business_id, staff_id) REFERENCES staff (business_id, id) ON DELETE CASCADE,
    FOREIGN KEY (business_id, service_id) REFERENCES services (business_id, id) ON DELETE CASCADE
);
CREATE INDEX staff_services_service ON staff_services (business_id, service_id);

-- Weekly hours in the business's wall-clock time: minutes of the day, [start, end).
CREATE TABLE weekly_hours (
    business_id uuid NOT NULL,
    staff_id uuid NOT NULL,
    weekday smallint NOT NULL CHECK (weekday BETWEEN 1 AND 7),
    start_minute smallint NOT NULL CHECK (start_minute BETWEEN 0 AND 1439),
    end_minute smallint NOT NULL CHECK (end_minute BETWEEN 1 AND 1440),
    PRIMARY KEY (business_id, staff_id, weekday, start_minute),
    CHECK (start_minute < end_minute),
    FOREIGN KEY (business_id, staff_id) REFERENCES staff (business_id, id) ON DELETE CASCADE
);

-- Dates first_date..=last_date: closed (a staff member, or everyone when staff_id is null;
-- the whole day or a minute range), or open (a staff member's hours for those dates,
-- replacing the weekly hours).
CREATE TABLE hours_exceptions (
    business_id uuid NOT NULL REFERENCES businesses (workspace_id) ON DELETE CASCADE,
    id uuid NOT NULL,
    staff_id uuid,
    first_date date NOT NULL,
    last_date date NOT NULL,
    kind text NOT NULL CHECK (kind IN ('closed', 'open')),
    start_minute smallint CHECK (start_minute BETWEEN 0 AND 1439),
    end_minute smallint CHECK (end_minute BETWEEN 1 AND 1440),
    note text CHECK (char_length(note) <= 200),
    PRIMARY KEY (business_id, id),
    CHECK (first_date <= last_date AND last_date - first_date <= 366),
    CHECK ((start_minute IS NULL) = (end_minute IS NULL)),
    CHECK (start_minute IS NULL OR start_minute < end_minute),
    CHECK (kind = 'closed' OR (staff_id IS NOT NULL AND start_minute IS NOT NULL)),
    FOREIGN KEY (business_id, staff_id) REFERENCES staff (business_id, id) ON DELETE CASCADE
);
CREATE INDEX hours_exceptions_dates ON hours_exceptions (business_id, last_date);

-- A booking is created `held` (with an expiry) or `confirmed`. `blocked` is the range the
-- staff member is unavailable: the service plus both buffers, computed here from the
-- service row, never taken from the caller.
CREATE TABLE bookings (
    business_id uuid NOT NULL REFERENCES businesses (workspace_id) ON DELETE CASCADE,
    id uuid NOT NULL,
    staff_id uuid NOT NULL,
    service_id uuid NOT NULL,
    status text NOT NULL CHECK (status IN ('held', 'confirmed', 'cancelled', 'expired')),
    starts_at timestamptz NOT NULL,
    ends_at timestamptz NOT NULL,
    buffer_before_minutes smallint NOT NULL,
    buffer_after_minutes smallint NOT NULL,
    blocked tstzrange NOT NULL,
    hold_expires_at timestamptz,
    customer_name text NOT NULL CHECK (char_length(customer_name) BETWEEN 1 AND 200),
    customer_email text CHECK (char_length(customer_email) BETWEEN 3 AND 254),
    idempotency_key text NOT NULL CHECK (idempotency_key ~ '^[A-Za-z0-9._:-]{1,64}$'),
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    PRIMARY KEY (business_id, id),
    UNIQUE (business_id, idempotency_key),
    CHECK (status <> 'held' OR hold_expires_at IS NOT NULL),
    CHECK (NOT isempty(blocked) AND lower_inc(blocked) AND NOT upper_inc(blocked)),
    FOREIGN KEY (business_id, staff_id) REFERENCES staff (business_id, id),
    FOREIGN KEY (business_id, service_id) REFERENCES services (business_id, id),
    -- The guarantee: for one staff member, no two live bookings' blocked ranges overlap.
    -- A hold past its expiry still matches the predicate until it is marked expired, which
    -- bookings_prepare does inside the transaction of any booking that needs its range.
    CONSTRAINT bookings_no_overlap EXCLUDE USING gist (
        business_id WITH =, staff_id WITH =, blocked WITH && -- LIMIT(no-overlap)
    ) WHERE (status IN ('held', 'confirmed'))
);
CREATE INDEX bookings_starts ON bookings (business_id, starts_at);

-- Errors raised for the server to map (class BK):
--   BK001 the hold has expired          BK002 the service or staff member is inactive
--   BK003 the staff member does not perform the service
--   BK004 a booking changes only its status    BK005 that status change is not allowed
CREATE FUNCTION public.bookings_prepare() RETURNS trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    svc record;
BEGIN
    SELECT s.duration_minutes, s.buffer_before_minutes, s.buffer_after_minutes, s.active
      INTO svc
      FROM public.services s
     WHERE s.business_id = NEW.business_id AND s.id = NEW.service_id;
    IF NOT FOUND THEN
        RAISE foreign_key_violation USING MESSAGE = 'no such service';
    END IF;
    IF NOT svc.active OR NOT EXISTS (
        SELECT 1 FROM public.staff m
         WHERE m.business_id = NEW.business_id AND m.id = NEW.staff_id AND m.active) THEN
        RAISE EXCEPTION 'the service or staff member is inactive' USING ERRCODE = 'BK002';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM public.staff_services a
         WHERE a.business_id = NEW.business_id AND a.staff_id = NEW.staff_id
           AND a.service_id = NEW.service_id) THEN
        RAISE EXCEPTION 'the staff member does not perform the service' USING ERRCODE = 'BK003';
    END IF;
    NEW.buffer_before_minutes := svc.buffer_before_minutes;
    NEW.buffer_after_minutes := svc.buffer_after_minutes;
    NEW.ends_at := NEW.starts_at + make_interval(mins => svc.duration_minutes);
    NEW.blocked := tstzrange(
        NEW.starts_at - make_interval(mins => svc.buffer_before_minutes),
        NEW.ends_at + make_interval(mins => svc.buffer_after_minutes),
        '[)');
    NEW.created_at := now();
    NEW.updated_at := now();
    IF NEW.status = 'held' THEN
        IF NEW.hold_expires_at IS NULL OR NEW.hold_expires_at <= now() THEN
            RAISE EXCEPTION 'the hold has expired' USING ERRCODE = 'BK001';
        END IF;
    ELSIF NEW.status = 'confirmed' THEN
        NEW.hold_expires_at := NULL;
    ELSE
        RAISE EXCEPTION 'a booking starts held or confirmed' USING ERRCODE = 'BK005';
    END IF;
    -- Expiry, enforced here: a hold past its expiry that overlaps this booking is marked
    -- expired in this transaction, so it leaves the constraint's predicate before this
    -- row's index entry is checked. Concurrent bookings serialise on the hold's row lock.
    UPDATE public.bookings b
       SET status = 'expired'
     WHERE b.business_id = NEW.business_id AND b.staff_id = NEW.staff_id
       AND b.status = 'held' AND b.hold_expires_at <= now()
       AND b.blocked && NEW.blocked;
    RETURN NEW;
END;
$$;

CREATE FUNCTION public.bookings_transition() RETURNS trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF (NEW.business_id, NEW.id, NEW.staff_id, NEW.service_id, NEW.starts_at, NEW.ends_at,
        NEW.buffer_before_minutes, NEW.buffer_after_minutes, NEW.blocked, NEW.hold_expires_at,
        NEW.customer_name, NEW.customer_email, NEW.idempotency_key, NEW.created_at)
       IS DISTINCT FROM
       (OLD.business_id, OLD.id, OLD.staff_id, OLD.service_id, OLD.starts_at, OLD.ends_at,
        OLD.buffer_before_minutes, OLD.buffer_after_minutes, OLD.blocked, OLD.hold_expires_at,
        OLD.customer_name, OLD.customer_email, OLD.idempotency_key, OLD.created_at) THEN
        RAISE EXCEPTION 'a booking changes only its status' USING ERRCODE = 'BK004';
    END IF;
    IF NEW.status = OLD.status THEN
        RETURN NEW;
    END IF;
    IF OLD.status = 'held' AND NEW.status = 'confirmed' THEN
        IF OLD.hold_expires_at <= now() THEN
            RAISE EXCEPTION 'the hold has expired' USING ERRCODE = 'BK001';
        END IF;
    ELSIF OLD.status = 'held' AND NEW.status = 'expired' THEN
        IF OLD.hold_expires_at > now() THEN
            RAISE EXCEPTION 'the hold has not expired' USING ERRCODE = 'BK005';
        END IF;
    ELSIF NOT (OLD.status IN ('held', 'confirmed') AND NEW.status = 'cancelled') THEN
        RAISE EXCEPTION 'a booking may not go from % to %', OLD.status, NEW.status
            USING ERRCODE = 'BK005';
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER bookings_prepare BEFORE INSERT ON bookings
    FOR EACH ROW EXECUTE FUNCTION bookings_prepare();
CREATE TRIGGER bookings_transition BEFORE UPDATE ON bookings
    FOR EACH ROW EXECUTE FUNCTION bookings_transition();
ALTER TABLE bookings ENABLE ALWAYS TRIGGER bookings_prepare;
ALTER TABLE bookings ENABLE ALWAYS TRIGGER bookings_transition;

GRANT SELECT, INSERT, UPDATE, DELETE
    ON businesses, services, staff, staff_services, weekly_hours, hours_exceptions
    TO booking_app;
-- No DELETE: a booking is cancelled or expires; its row stays.
GRANT SELECT, INSERT, UPDATE ON bookings TO booking_app;
