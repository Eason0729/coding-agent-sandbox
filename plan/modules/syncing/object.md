Implement ObjectStore

## Object Path Sharding

Object IDs are 64-bit unsigned integers. Objects are stored at:

```
<objects_dir>/<shard>/<id_hex>
```

- **shard**: lowercase hex of low byte (`id & 0xff`), padded to 2 chars (`00`..`ff`)
- **id_hex**: full 16-char lowercase hex representation (`{:016x}`)

Example: object ID `1` → `objects/00/0000000000000001`

This ensures incremental IDs spread across 256 shards, avoiding a hotspot in `00`.
