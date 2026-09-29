# documents_fixture

A pipeline-shaped documents pack: one `worker` slot, one tooled behavior with
a read-only host workspace, one schema, and one filtered event source. Used
by scenario, eval, proposer-refusal and `gents pack check` tests that need a
real, minimal documents pack rather than one of the official ones.
