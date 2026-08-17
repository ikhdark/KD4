ALTER TABLE mutation_files
ADD COLUMN start_epoch INTEGER NOT NULL DEFAULT 0 CHECK (start_epoch >= 0);

ALTER TABLE mutation_files
ADD COLUMN end_epoch INTEGER CHECK (end_epoch IS NULL OR end_epoch >= start_epoch);
