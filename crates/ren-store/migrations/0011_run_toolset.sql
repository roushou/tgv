-- The toolset generation on a run's search path (`ren tools`): 0 when Ren
-- had installed none. NULL when the tool store could not be read at its
-- start, and for runs started before this was recorded.
ALTER TABLE runs ADD COLUMN toolset_generation INTEGER;
