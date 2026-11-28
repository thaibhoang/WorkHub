-- Autopilot mode (TaskCreated.autopilot / TaskActivated.autopilot): gates and agent
-- questions are decided by the autopilot agent instead of a human.
ALTER TABLE tasks_view ADD COLUMN autopilot BOOLEAN NOT NULL DEFAULT FALSE;
