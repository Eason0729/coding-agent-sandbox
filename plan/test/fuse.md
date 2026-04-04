Create a reproducible bash script for testing fuse.

The bash script run from project root, use `./tmp` for testing storage.

The integration test should include:
1. basic write
2. copy-on-write
3. mmap(skip for now)
4. copy-on-write with sqlite3
5. sparse/random small writes performance sanity check (seek+small-write pattern)

Additionally, add pure Rust unit tests for behavior tables:
1. stat behavior matrix for all access modes including whiteout
2. open behavior matrix including first-write copy-up and truncate handling
3. readdir child-resolution matrix over (real present, fuse present, whiteout, mode)
4. passthrough non-whiteout collision classified as DontCareCollision

Additionally, add pure Rust unit tests for explicit open transition traces:
1. read-only CoW open with no fuse object resolves to real open only
2. first write CoW open on existing real file includes copy-up before open
3. write CoW open with truncate skips copy-up
4. whiteout path always resolves to not-found transition
