Implement `shm/mutex.rs` — `ShmGuard` (RAII mutex guard) and `adopt_mutex_after_fork` (reinitialize mutex after fork).

## Note

- `pthread_mutex` is not valid after `fork` in the child — the child must reinitialize it before using.

---

## `ShmGuard`

A simple RAII guard that holds the SHM mutex.

```rust
pub struct ShmGuard {
    mutex: *mut pthread_mutex_t,
}

impl ShmGuard {
    pub fn new(mutex: *mut pthread_mutex_t) -> Result<Self> {
        unsafe {
            let ret = pthread_mutex_lock(mutex);
            if ret != 0 {
                return Err(Error::from_raw(ret));
            }
        }
        Ok(Self { mutex })
    }
}

impl Drop for ShmGuard {
    fn drop(&mut self) {
        unsafe {
            pthread_mutex_unlock(self.mutex);
        }
    }
}
```

The guard is **not** `Send` or `Sync` — it must stay in the process that acquired it.

---

## `adopt_mutex_after_fork`

Called by the **child process** (the syncing daemon) after `fork`. The child inherits the parent's memory mapping but the mutex state is undefined after fork.

### Important: Must destroy before reinitializing

The mutex was locked by the parent (who then called `forget` on the guard to not unlock it). The child **must** destroy this broken state before reinitializing, otherwise the mutex may be in an undefined/inconsistent state.

```rust
pub unsafe fn adopt_mutex_after_fork(state: &mut ShmStateLayout) -> Result<()>
```

### Steps

1. **Destroy the inherited mutex**

   The mutex was locked by the parent (who then called `forget` on the guard to not unlock it). The child must destroy this broken state:

   ```rust
   pthread_mutex_destroy(state.mutex_ptr())?;
   ```

2. **Reinitialize with process-shared attributes**

   Create attributes with `PTHREAD_PROCESS_SHARED`:

   ```rust
   let mut attr: pthread_mutexattr_t = mem::zeroed();
   pthread_mutexattr_init(&mut attr)?;
   pthread_mutexattr_setpshared(&mut attr, PTHREAD_PROCESS_SHARED)?;
   ```

3. **Initialize the mutex**

   ```rust
   pthread_mutex_init(state.mutex_ptr(), &attr)?;
   pthread_mutexattr_destroy(&mut attr)?;
   ```

4. **Lock and unlock** to verify

   ```rust
   pthread_mutex_lock(state.mutex_ptr())?;
   pthread_mutex_unlock(state.mutex_ptr())?;
   ```

   This is necessary because `pthread_mutex_init` leaves the mutex in an unlocked state, but the parent's lock is still "valid" from the kernel's perspective. We need a clean state.

---

## Usage in the Fork Protocol

From the overview:

```
1. cas run locks SHM mutex, increments running_count
2. If prev == 0:
   a. fork child (syncing daemon)
   b. parent forget(guard) — does NOT unlock
   c. child:
        adopt_mutex_after_fork(state)
        bind socket
        state.set_socket_ready(true)
        // child is now the "lock holder" — unlock to let others proceed
        unlock mutex
```

The lock transfer works because:
- Parent held the lock, called `forget` (never unlocked)
- Child reinitialized the mutex (making it "unlocked" from its perspective)
- Child locked it immediately after init
- Child unlocked after setting `socket_ready = 1`
- Now other `cas run` processes can acquire the lock

---

## Edge Cases

- **Parent exits before child sets socket_ready**: Other `cas run` processes spin forever. The parent should wait for `socket_ready` before releasing the lock (but note: parent used `forget`, so it never releases). The protocol has a race here — see the fix below.

- **Fix**: The parent should **not** use `forget`. Instead:
  1. Parent locks mutex
  2. Increments count
  3. If prev == 0: fork child
  4. Parent unlocks mutex (regardless of whether it forked)
  5. Parent spins on `socket_ready`
  
  Wait — the protocol says parent `forget`s the lock. But then how does the child acquire it?

  The correct protocol:
  - Parent locks
  - Parent increments count
  - If prev == 0: fork child
  - Parent unlocks (always — not forget)
  - Parent spins on socket_ready
  - Child (after fork): adopts mutex, locks, sets socket_ready=1, unlocks

  This ensures proper handoff. The "forget" in the original protocol was wrong — it would leave the mutex in a held state.

---

## Safety Notes

- `adopt_mutex_after_fork` must only be called once per process in the child
- The mutex must be process-shared (`PTHREAD_PROCESS_SHARED`) for cross-process coordination to work
- All users of the SHM must use the same layout and initialization
