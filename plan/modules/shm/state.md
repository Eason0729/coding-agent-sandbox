Implement `shm/state.rs` — `ShmState`, a typed accessor over the shared memory layout.

This module defines the binary layout of the SHM region and provides safe getters/setters. The actual synchronization (mutex) is handled in `mutex.rs`.

---

## Goals

1. Define the layout of the shared memory region
2. Provide atomic accessors for `running_count` and `socket_ready`
3. Provide a raw pointer to the embedded mutex for `mutex.rs`

---

## Layout

The SHM region contains:

```
+----------------+----------------+----------------+
| mutex (40B)    | running_count  | socket_ready   |
| (pthread_mutex_t, process-shared) | (u32)      | (AtomicU32)   |
+----------------+----------------+----------------+
0                40               44               48
```

Size: 48 bytes minimum (rounded up to 64 for alignment).

## Notice on race-condition

Although it's possible to use atomic fetch_add(it's basically a counter). We use lock to prevent race-condition. HOLD lock during transaction!(For example: you are doing to start syncing daemon)
