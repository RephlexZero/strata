-- Stream end attribution (UX_TRUST_AUDIT U2/U7).
--
-- end_reason: the device-reported StreamEndReason (pipeline_crash, error,
-- control_plane_stop, …) or a control-plane inferred cause. Previously the
-- reason was discarded and every end rendered identically.
--
-- end_inferred: TRUE only for ends the control plane *inferred* (reconcile,
-- sweep, stop timeout) rather than observed from a device or operator.
-- Readoption keys off this flag; it used to key off error_message IS NOT
-- NULL, which broke once real crash reasons were persisted there.
--
-- restarted_from: lineage — the stream this one replaced, so the dashboard
-- can show stop→start sequences as a continuation rather than an unrelated
-- new broadcast.
ALTER TABLE streams ADD COLUMN IF NOT EXISTS end_reason TEXT;
ALTER TABLE streams ADD COLUMN IF NOT EXISTS end_inferred BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE streams ADD COLUMN IF NOT EXISTS restarted_from TEXT;

-- Backfill: every historical error_message was written by an inferred path.
UPDATE streams SET end_inferred = TRUE WHERE error_message IS NOT NULL;
