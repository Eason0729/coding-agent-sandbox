import { describe, it } from "node:test";
import assert from "node:assert";
import { shellQuote, isHostEscape, buildCasWrappedCommand, resolveWorkdir, stripHostPrefix } from "../../plugin/helpers.js";

describe("shellQuote", () => {
  it("plain alphanumeric needs no quoting", () => {
    assert.equal(shellQuote("hello"), "hello");
    assert.equal(shellQuote("hello-world"), "hello-world");
    assert.equal(shellQuote("path/to/file"), "path/to/file");
    assert.equal(shellQuote("foo.bar:8080"), "foo.bar:8080");
    assert.equal(shellQuote("src/main.rs"), "src/main.rs");
  });

  it("single quote gets escaped", () => {
    assert.equal(shellQuote("it's"), "'it'\"'\"'s'");
    assert.equal(shellQuote("don't"), "'don'\"'\"'t'");
  });

  it("spaces require quoting", () => {
    assert.equal(shellQuote("hello world"), "'hello world'");
    assert.equal(shellQuote("/path with spaces"), "'/path with spaces'");
  });

  it("shell metacharacters get quoted", () => {
    assert.equal(shellQuote("foo; bar"), "'foo; bar'");
    assert.equal(shellQuote("$(whoami)"), "'$(whoami)'");
    assert.equal(shellQuote("`ls`"), "'`ls`'");
    assert.equal(shellQuote("foo|bar"), "'foo|bar'");
  });

  it("empty string becomes two single quotes", () => {
    assert.equal(shellQuote(""), "''");
  });
});

describe("isHostEscape", () => {
  it("detects HOST: prefix case-insensitively", () => {
    assert.equal(isHostEscape("HOST: git status"), true);
    assert.equal(isHostEscape("host: ls"), true);
    assert.equal(isHostEscape("Host: echo hello"), true);
  });

  it("detects HOST: with no space", () => {
    assert.equal(isHostEscape("HOST:ls"), true);
  });

  it("detects HOST: with multiple spaces after colon", () => {
    assert.equal(isHostEscape("host:  ls"), true);
  });

  it("returns false for plain commands", () => {
    assert.equal(isHostEscape("git status"), false);
    assert.equal(isHostEscape("ls -la"), false);
  });

  it("returns false for HOST inside quotes", () => {
    assert.equal(isHostEscape("bash -c 'HOST: ls'"), false);
  });

  it("returns false for empty or non-string", () => {
    assert.equal(isHostEscape(""), false);
    assert.equal(isHostEscape(null), false);
    assert.equal(isHostEscape(undefined), false);
    assert.equal(isHostEscape(123), false);
  });
});

describe("buildCasWrappedCommand", () => {
  it("wraps simple command", () => {
    const result = buildCasWrappedCommand("/project", "ls -la");
    assert.equal(result, "cas --root /project run bash -lc 'ls -la'");
  });

  it("quotes workdir with spaces", () => {
    const result = buildCasWrappedCommand("/path with spaces", "ls");
    assert.equal(result, "cas --root '/path with spaces' run bash -lc ls");
  });

  it("defaults to process.cwd for empty workdir", () => {
    const result = buildCasWrappedCommand("", "echo hello");
    const cwd = process.cwd();
    assert.ok(result.startsWith(`cas --root ${cwd}`));
    assert.ok(result.includes("run bash -lc 'echo hello'"));
  });

  it("escapes command with single quotes", () => {
    const result = buildCasWrappedCommand("/project", "echo 'it's working'");
    assert.ok(result.includes("cas --root /project"));
    assert.ok(result.includes("run bash -lc"));
  });
});

describe("resolveWorkdir", () => {
  it("returns trimmed workdir if non-empty", () => {
    assert.equal(resolveWorkdir("  /project  "), "/project");
  });

  it("returns process.cwd for undefined", () => {
    assert.equal(resolveWorkdir(undefined), process.cwd());
  });

  it("returns process.cwd for empty string", () => {
    assert.equal(resolveWorkdir(""), process.cwd());
  });

  it("returns process.cwd for whitespace-only string", () => {
    assert.equal(resolveWorkdir("   "), process.cwd());
  });
});

describe("stripHostPrefix", () => {
  it("strips HOST: prefix", () => {
    assert.equal(stripHostPrefix("HOST: git status"), "git status");
  });

  it("strips host: prefix and trims", () => {
    assert.equal(stripHostPrefix("host:  ls"), "ls");
  });

  it("handles uppercase HOST", () => {
    assert.equal(stripHostPrefix("HOST:echo hello"), "echo hello");
  });
});
