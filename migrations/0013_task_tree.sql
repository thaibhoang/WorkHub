-- Task tree and drafts: tasks_view.parent_id (TaskCreated.parent / TaskMoved) and
-- status 'draft' (TaskCreated.draft until TaskActivated).
ALTER TABLE tasks_view ADD COLUMN parent_id UUID;
CREATE INDEX tasks_view_parent_idx ON tasks_view (parent_id);
