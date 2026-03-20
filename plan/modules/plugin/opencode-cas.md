# opencode-cas Plugin Specification

## What Is This?

`opencode-cas` is an OpenCode plugin that intercepts every `bash` tool call and automatically runs it inside a CAS (Coding Agent Sandbox) filesystem isolation environment, providing safe execution for AI coding agents without any user configuration.

## Goals

- Intercept all `bash` tool executions and route them through `cas run` by default.
- Provide a simple escape hatch (`HOST:` prefix) for users who need to run commands on the host.
- Require zero configuration for the common case — just install and it works.
- Detect `cas` binary availability and provide clear error messages.

## Non-Goals

- Network isolation (handled by CAS, not this plugin).
- Container-level isolation beyond what CAS provides via FUSE.
- Modifying non-bash tools — only `bash` is intercepted.
- Interactive terminal handling — CAS wraps shell commands, not raw TTY sessions.

---

## Hook Contract

### `tool.execute.before`

Fires before any tool executes. Receives:

```typescript
input: {
  tool: string;        // tool name e.g. "bash", "read"
  sessionID: string;
  args: Record<string, unknown>;
}
output: {
  args: {
    command?: string;  // bash command string
    workdir?: string;  // working directory
    [key: string]: unknown;
  };
}
```

**Plugin logic pseudocode:**

```
IF input.tool != "bash":
  RETURN  // passthrough unchanged

cmd := output.args.command
IF cmd is empty OR only whitespace:
  RETURN  // nothing to wrap

workdir := output.args.workdir ?? process.cwd()

IF cmd.trim().toUpperCase().startsWith("HOST:"):
  output.args.command := cmd.trim()[5:].trimStart()
  RETURN  // run on host, unwrapped

output.args.command := buildCasWrappedCommand(workdir, cmd)
```

### `buildCasWrappedCommand(workdir, cmd) -> string`

Returns a shell command string:

```
cas --root <shellQuote(workdir)> run bash -lc <shellQuote(cmd)>
```

Both `workdir` and `cmd` are shell-quoted using `shellQuote()`.

---

## Shell Quoting Strategy

Uses single-quote wrapping with `"'"` pattern (same as `opencode-devcontainers`):

```
shellQuote(str):
  IF str matches /^[a-zA-Z0-9_\-./=:@]+$/:
    RETURN str  // no quoting needed
  RETURN "'" + str.replace(/'/g, "'\"'\"'") + "'"
```

This prevents all shell interpretation except for single quotes themselves.

---

## Escape Hatch

`HOST:` prefix (case-insensitive):

```
HOST: git status    →  git status  (unwrapped, on host)
host: ls -la        →  ls -la       (unwrapped, on host)
```

Stripped before passing to bash. No CAS involvement.

---

## Failure Modes

| Scenario | Behavior |
|---|---|
| `cas` binary not found | Return clear error message: `"cas not found. Install from https://github.com/eason-cas/cas"` |
| `cas run` fails (non-zero exit) | Pass through the original error output from CAS; do not hide it |
| Empty command | Passthrough unchanged (let OpenCode handle it) |
| `workdir` contains spaces | Quoted correctly by `shellQuote` |
| `cas init` not run yet | CAS will auto-init on first `cas run` (documented behavior of CAS) |

---

## File Structure

```
plugins/opencode-cas/
├── package.json
├── README.md
├── opencode.json          # example config for local dev
├── plugin/
│   ├── index.js           # plugin entry point
│   └── helpers.js         # shellQuote, buildCasWrappedCommand, etc.
└── test/
    └── unit/
        ├── index.test.js
        └── helpers.test.js
```

---

## Interaction with opencode-devcontainers

This plugin is orthogonal to `opencode-devcontainers`:

- `opencode-devcontainers`: isolates workspace at the container/git-worktree level
- `opencode-cas`: isolates filesystem access via FUSE CoW for every bash call

Both can coexist. The execution order would be:
1. `opencode-cas` rewrites bash → `cas run bash -lc "..."`  
2. CAS's FUSE layer handles filesystem isolation

If both are active, CAS runs inside whatever workspace context `opencode-devcontainers` has set (via `workdir`).

---

## Dependencies

- `@opencode-ai/plugin` (peer) — for `tool()` helper and plugin types
- Node.js ≥ 18 (matches OpenCode engine requirement)

No native dependencies. Pure JavaScript.

---

## v1 Scope

Single hook: `tool.execute.before` on `bash`. No custom tools, no additional events.
