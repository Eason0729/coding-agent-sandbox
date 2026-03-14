Implement `shm/mutex.rs` — `ShmGuard` (RAII mutex guard) and `adopt_mutex_after_fork` (reinitialize mutex after fork).

## Note

- `pthread_mutex` is not valid after `fork` in the child — the child must reinitialize it before using.
