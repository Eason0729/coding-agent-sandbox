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
+-----------------------------------+----------------+---------------+
|             mutex (40B)           | running_count  | socket_ready  |
| (pthread_mutex_t, process-shared) |     (u32)      | (AtomicU32)   |
+-----------------------------------+----------------+---------------+
0                                   40               44             48
```

Size: 48 bytes minimum (rounded up to 64 for alignment).

## IMPORTANT Notice on race-condition

Although it's possible to use atomic fetch_add(it's basically a counter). We use lock to prevent race-condition. HOLD lock during transaction!(For example: you are doing to start syncing daemon)

Condition here include not only the counter itself but also include state like syncing daemon is starting up or shutting down.

For example, consider syncing daemon is shutting down, but two new client is connecting:
1. Syncing daemon acquire lock to shutdown
2. New client A try acquire lock, but syncing daemon is shutting down.
3. New client B try acquire lock, but syncing daemon is shutting down.
4. Syncing daemon shutdown and exit.
5. New client A found that socket is not ready, so it acquire lock and start a new one.
6. New client A started the server.
7. New client B try acquire lock, and found the socket is ready.

The magic here is that we would never poll socket to check syncing server is alive.

Consider following case where fetch_add is used for increment counter(WRONG):
1. Syncing daemon is shutting down.
2. New Client found that socket is alive by polling(syncing daemon has not finish shutting down yet).
3. New Client started, and make connection.
4. Syncing daemon finish shutdown down
5. New Client throw error we actually send request, the socket is dead!
