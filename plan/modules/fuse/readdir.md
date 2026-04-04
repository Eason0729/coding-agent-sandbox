# fuse/readdir decision table

## Child state columns

Per child name under a directory, classify using:

- real child present?
- fuse child present?
- fuse child is whiteout?
- access mode

## Expected child-level outcome

| AccessMode | Real child | Fuse child | Whiteout | Outcome |
|---|---:|---:|---:|---|
| Passthrough | N | N | N | Hide |
| Passthrough | Y | N | N | ShowReal |
| Passthrough | N | Y | N | ShowFuse |
| Passthrough | Y | Y | N | DontCareCollision |
| Passthrough | N | Y | Y | Hide |
| Passthrough | Y | Y | Y | Hide |
| FuseOnly | N | N | N | Hide |
| FuseOnly | Y | N | N | Hide |
| FuseOnly | N | Y | N | ShowFuse |
| FuseOnly | Y | Y | N | ShowFuse |
| FuseOnly | N | Y | Y | Hide |
| FuseOnly | Y | Y | Y | Hide |
| CopyOnWrite | N | N | N | Hide |
| CopyOnWrite | Y | N | N | ShowReal |
| CopyOnWrite | N | Y | N | ShowFuse |
| CopyOnWrite | Y | Y | N | ShowFuse |
| CopyOnWrite | N | Y | Y | Hide |
| CopyOnWrite | Y | Y | Y | Hide |

Notes:

- Whiteout strictly masks the child name.
- In `Passthrough`, non-whiteout collision is classified as `DontCareCollision`.
