-- Stop intent is separate from status: a rejected status write must not
-- silently reactivate an unresolved stop after runtime reconstruction.
ALTER TABLE thread_goals ADD COLUMN terminal_transition TEXT;
