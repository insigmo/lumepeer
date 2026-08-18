-- Device conflict resolution (design doc §12.2, §19 phase 3).
--
-- A license has a fixed number of seats. When one more device asks for a token
-- than the plan seats, the device with the oldest heartbeat is displaced: it
-- keeps its row, so its next heartbeat can be told why it lost, instead of
-- silently disappearing and looking like a database fault to the client.

ALTER TABLE devices ADD COLUMN displaced_at INTEGER;
