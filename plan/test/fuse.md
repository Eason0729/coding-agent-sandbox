Create a reproducible bash script for testing fuse.

The bash script run from project root, use `./tmp` for testing storage.

The test should include:
1. basic write
2. copy-on-write
2. mmap(skip for now)
3. copy-on-write with sqlite3
4. sparse/random small writes performance sanity check (seek+small-write pattern)
