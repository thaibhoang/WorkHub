-- Identity of this event store. Written as a comment on the task databases it
-- creates, so cleanup never drops another store's. Not a read model.
CREATE TABLE instance (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    id        uuid    NOT NULL DEFAULT gen_random_uuid()
);
INSERT INTO instance DEFAULT VALUES;
