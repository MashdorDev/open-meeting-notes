-- Names of the people on the other side of a call, found in the transcript
-- after it is saved. JSON array of strings; NULL when none were found.
ALTER TABLE meetings ADD COLUMN other_speaker_names TEXT;
