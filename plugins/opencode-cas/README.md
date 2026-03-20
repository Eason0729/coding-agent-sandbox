# opencode-cas

OpenCode plugin for safe bash execution via CAS (Coding Agent Sandbox) filesystem isolation.

## What Does It Do?

Every time OpenCode's AI runs a `bash` tool, this plugin automatically wraps it with `cas run`:

```
# Without plugin
bash -lc "npm install"

# With plugin (automatically)
cas --root /project run bash -lc 'npm install'
```

The AI's filesystem operations are then isolated by CAS's FUSE layer:
- Reads go to the real filesystem
- Writes are copy-on-write to `.sandbox/data/`
- Blocked paths (e.g. `.env`) are hidden

## Installation

### From npm

Add to your `~/.config/opencode/opencode.json`:

```json
{
  "plugin": ["opencode-cas"]
}
```

OpenCode automatically installs npm plugins on startup.

### For local development

```json
{
  "plugin": ["/path/to/cas/plugins/opencode-cas"]
}
```

## Escape Hatch

Prefix any command with `HOST:` to run it on the host, bypassing CAS:

```
HOST: git status   # runs directly on host, not sandboxed
HOST: ls -la        # host execution
```

## How It Works

1. The plugin intercepts every `bash` tool call via the `tool.execute.before` hook
2. Rewrites the command to: `cas --root <workdir> run bash -lc '<original command>'`
3. CAS's FUSE filesystem provides copy-on-write isolation for all file operations

## Requirements

- [CAS](https://github.com/eason-cas/cas) installed and available in `$PATH`
- Linux kernel with FUSE support
- Node.js ≥ 18

## Example

```json
// ~/.config/opencode/opencode.json
{
  "$schema": "https://opencode.ai/config.json",
  "plugin": ["opencode-cas"]
}
```

Then in an OpenCode session:

```
> npm install
  → cas --root /project run bash -lc 'npm install'

> HOST: git status
  → git status
```
