import { describe, it } from "node:test";
import assert from "node:assert";
import { opencodeCas } from "../../plugin/index.js";

function makeCtx() {
  return {
    client: { app: { log: () => {} } },
    project: {},
    directory: "/project",
    worktree: null,
  };
}

function makeInput(tool, args = {}) {
  return { tool, sessionID: "test-session", args };
}

function makeOutput(args = {}) {
  return { args: { ...args } };
}

describe("opencodeCas hook", () => {
  it("passthru non-bash tools unchanged", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("read", { filePath: "src/main.rs" });
    const output = makeOutput({ filePath: "src/main.rs" });

    await handler(input, output);

    assert.equal(output.args.filePath, "src/main.rs");
    assert.equal(output.args.command, undefined);
  });

  it("wraps bash command in cas run", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("bash", { command: "ls -la", workdir: "/project" });
    const output = makeOutput({ command: "ls -la", workdir: "/project" });

    await handler(input, output);

    assert.ok(output.args.command.startsWith("cas --root /project run bash -lc"));
    assert.ok(output.args.command.includes("ls -la"));
  });

  it("strips HOST: prefix and bypasses cas wrapping", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("bash", { command: "HOST: git status", workdir: "/project" });
    const output = makeOutput({ command: "HOST: git status", workdir: "/project" });

    await handler(input, output);

    assert.equal(output.args.command, "git status");
    assert.equal(output.args.workdir, "/project");
  });

  it("strips host: lowercase prefix", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("bash", { command: "host: ls", workdir: "/project" });
    const output = makeOutput({ command: "host: ls", workdir: "/project" });

    await handler(input, output);

    assert.equal(output.args.command, "ls");
  });

  it("leaves empty command unchanged", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("bash", { command: "   ", workdir: "/project" });
    const output = makeOutput({ command: "   ", workdir: "/project" });

    await handler(input, output);

    assert.equal(output.args.command, "   ");
  });

  it("uses process.cwd when workdir is absent", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("bash", { command: "ls" });
    const output = makeOutput({ command: "ls" });

    await handler(input, output);

    const cwd = process.cwd();
    assert.ok(output.args.command.startsWith(`cas --root ${cwd} run bash -lc`));
  });

  it("quotes workdir with spaces", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input = makeInput("bash", { command: "ls", workdir: "/path with spaces" });
    const output = makeOutput({ command: "ls", workdir: "/path with spaces" });

    await handler(input, output);

    assert.ok(output.args.command.includes("'/path with spaces'"));
  });

  it("handles multiple bash calls independently", async () => {
    const plugin = await opencodeCas(makeCtx());
    const handler = plugin["tool.execute.before"];

    const input1 = makeInput("bash", { command: "ls", workdir: "/project" });
    const output1 = makeOutput({ command: "ls", workdir: "/project" });

    const input2 = makeInput("bash", { command: "HOST: git status", workdir: "/project" });
    const output2 = makeOutput({ command: "HOST: git status", workdir: "/project" });

    await handler(input1, output1);
    await handler(input2, output2);

    assert.ok(output1.args.command.startsWith("cas --root"));
    assert.equal(output2.args.command, "git status");
  });

  it("only exports tool.execute.before hook", async () => {
    const plugin = await opencodeCas(makeCtx());
    const keys = Object.keys(plugin);
    assert.deepEqual(keys, ["tool.execute.before"]);
  });
});
