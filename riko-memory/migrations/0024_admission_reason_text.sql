-- B2: reason_text generation and storage for candidates and memories.
-- 0001—0023 remain frozen.
ALTER TABLE memory_candidates ADD COLUMN reason_text TEXT;
ALTER TABLE memories ADD COLUMN reason_text TEXT;
