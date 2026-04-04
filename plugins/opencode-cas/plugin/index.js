import {
  shellQuote,
  isHostEscape,
  buildCasWrappedCommand,
  resolveWorkdir,
  stripHostPrefix,
} from "./helpers.js";

/**
 * @param {{ client: any, project: any, directory: string, worktree: string | null }} ctx
 */
export const opencodeCas = async (ctx) => {
  return {
    "tool.execute.before": async (input, output) => {
      if (input.tool !== "bash") {
        return;
      }

      const rawCommand = output.args?.command;
      if (!rawCommand || typeof rawCommand !== "string" || !rawCommand.trim()) {
        return;
      }

      const hostEscape = isHostEscape(rawCommand);
      if (hostEscape) {
        output.args.command = stripHostPrefix(rawCommand);
        return;
      }

      const workdir = resolveWorkdir(output.args?.workdir);
      output.args.command = buildCasWrappedCommand(workdir, rawCommand);
    },
  };
};

export default opencodeCas;
