-- Where each agent session ran (phase 7): 'worktree' or 'docker:<image>'.
-- NULL for sessions recorded before sandboxes existed (they ran in a worktree).
ALTER TABLE session_facts ADD COLUMN sandbox TEXT;
