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

## API

```rust
pub struct ObjectStore {
    dir: PathBuf,
    next_id: u64,
}

impl ObjectStore {
    pub fn new(dir: PathBuf, next_id: u64) -> Self;
    pub fn put(&mut self, data: &[u8]) -> Result<u64, ObjectError>;
    pub fn get(&self, id: u64) -> Result<Vec<u8>, ObjectError>;
    pub fn get_range(&self, id: u64, offset: u64, len: usize) -> Result<Vec<u8>, ObjectError>;
    pub fn exists(&self, id: u64) -> bool;
    pub fn init_dir(dir: &PathBuf) -> Result<(), ObjectError>;
}
```

## Error Types

```rust
#[derive(Error, Debug)]
pub enum ObjectError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Object not found: {0}")]
    NotFound(u64),
}
```
