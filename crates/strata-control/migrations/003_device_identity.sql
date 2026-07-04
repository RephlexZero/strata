-- E4: real device identity.
-- Receivers get the same ed25519 public-key column senders already had;
-- enrollment tokens become single-use (cleared once a device key is bound).
ALTER TABLE receivers ADD COLUMN device_public_key TEXT;
