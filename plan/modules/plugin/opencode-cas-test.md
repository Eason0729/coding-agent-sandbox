# opencode-cas Test Specification

## Unit Test Matrix — helpers.js

### `shellQuote`

| Input | Expected Output | Description |
|---|---|---|
| `"hello"` | `"hello"` | plain alphanumeric, no quoting |
| `"hello-world"` | `"hello-world"` | with dash, no quoting |
| `"path/to/file"` | `"path/to/file"` | with slash, no quoting |
| `"foo.bar:8080"` | `"foo.bar:8080"` | with colon and dot |
| `"it's"` | `"it'\"'\"'s"` | single quote escaped |
| `"hello world"` | `"'hello world'"` | space requires quoting |
| `"foo; bar"` | `"'foo; bar'"` | semicolon |
| `"$(whoami)"` | `"'$(whoami)'"` | command substitution |
| `""` | `"''"` | empty string becomes two single quotes |
| `"foo\nbar"` | `"'foo\nbar'"` | newline in string |
| `"/home/user with spaces"` | `"'/home/user with spaces'"` | path with spaces |

### `isHostEscape`

| Input | Expected | Description |
|---|---|---|
| `"HOST: git status"` | `true` | uppercase HOST |
| `"host: ls"` | `true` | lowercase host |
| `"HOST:echo hello"` | `true` | no space after colon |
| `"host:  ls"` | `true` | multiple spaces after colon |
| `"git status"` | `false` | no prefix |
| `"bash -c 'HOST: ls'"` | `false` | HOST inside quotes |
| `""` | `false` | empty string |

### `buildCasWrappedCommand`

| workdir | command | Expected |
|---|---|---|
| `"/project"` | `"ls -la"` | `cas --root /project run bash -lc 'ls -la'` |
| `"/path with spaces"` | `"ls"` | `cas --root '/path with spaces' run bash -lc 'ls'` |
| `"."` | `"echo hello"` | `cas --root . run bash -lc 'echo hello'` |
| `"/project"` | `"it's here"` | `cas --root /project run bash -lc 'it'\"'\"'s here'` |

---

## Unit Test Matrix — index.js (Hook Logic)

### Non-bash tool passthrough

For any `input.tool != "bash"`, output must be **identical** to input (no mutation).

Test with `input.tool = "read"`:
```
input = { tool: "read", args: { filePath: "src/main.rs" } }
output.args unchanged
```

### Bash command rewriting

**Case: simple command**
```
input.tool = "bash"
input.args = { command: "ls -la", workdir: "/project" }
→ output.args.command = "cas --root /project run bash -lc 'ls -la'"
```

**Case: HOST: prefix stripped**
```
input.tool = "bash"
input.args = { command: "HOST: git status", workdir: "/project" }
→ output.args.command = "git status"
→ output.args.workdir = undefined (unchanged)
```

**Case: empty command**
```
input.tool = "bash"
input.args = { command: "   ", workdir: "/project" }
→ output.args.command = "   " (unchanged)
```

**Case: no workdir**
```
input.tool = "bash"
input.args = { command: "ls", workdir: undefined }
→ output.args.command = "cas --root <process.cwd()> run bash -lc 'ls'"
```

**Case: workdir with spaces**
```
input.tool = "bash"
input.args = { command: "ls", workdir: "/path with spaces" }
→ output.args.command = "cas --root '/path with spaces' run bash -lc 'ls'"
```

**Case: command with single quotes**
```
input.tool = "bash"
input.args = { command: "echo 'it's working'", workdir: "/project" }
→ output.args.command = "cas --root /project run bash -lc 'echo '\"'\"'it'\"'\"'s working'\"'\"''"
```

---

## Integration Test Checklist

- [ ] Plugin loads without errors in Node.js ≥ 18
- [ ] `tool.execute.before` hook fires for `bash` tool only
- [ ] Non-bash tools pass through unchanged
- [ ] `HOST:` prefix correctly strips and bypasses CAS wrapping
- [ ] `cas` binary existence is NOT required for unit tests (mocked in tests)
- [ ] Package is `type: "module"` and uses ESM imports
- [ ] `opencode.json` example references correct npm package name
- [ ] `node --test` runs and passes all unit tests

---

## Publish/Install Verification

- [ ] `npm publish --dry-run` succeeds
- [ ] Package name is `opencode-cas` (or scoped `@scope/opencode-cas`)
- [ ] Main entry `plugin/index.js` is ESM
- [ ] README includes install instructions for `~/.config/opencode/opencode.json`
- [ ] No secret values or credentials in code
