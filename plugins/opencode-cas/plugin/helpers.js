/**
 * Shell-quotes a string for safe inclusion in a shell command.
 * Uses single quotes which prevent all shell interpretation except for
 * single quotes themselves, which are escaped using the '"'"' pattern.
 *
 * Returns the original string if it contains only safe characters.
 * @param {string} str
 * @returns {string}
 */
export function shellQuote(str) {
  if (/^[a-zA-Z0-9_\-./=:@]+$/.test(str)) {
    return str;
  }
  return "'" + str.replace(/'/g, "'\"'\"'") + "'";
}

/**
 * Checks if a command string starts with the HOST: escape hatch.
 * Matching is case-insensitive and allows optional whitespace after the colon.
 * @param {string} cmd
 * @returns {boolean}
 */
export function isHostEscape(cmd) {
  if (!cmd || typeof cmd !== "string") {
    return false;
  }
  const trimmed = cmd.trim();
  return /^HOST:/i.test(trimmed);
}

/**
 * Builds a CAS-wrapped bash command.
 * @param {string} workdir - Working directory (used as --root)
 * @param {string} command - Original bash command
 * @returns {string} - Full cas run command
 */
export function buildCasWrappedCommand(workdir, command) {
  const quotedWorkdir = shellQuote(workdir || process.cwd());
  const quotedCommand = shellQuote(command);
  return `cas --root ${quotedWorkdir} run bash -lc ${quotedCommand}`;
}

/**
 * Resolves the working directory for a bash command.
 * @param {string|undefined} workdirArg - workdir from output.args.workdir
 * @returns {string} - Resolved directory (defaults to process.cwd())
 */
export function resolveWorkdir(workdirArg) {
  return workdirArg && workdirArg.trim() ? workdirArg.trim() : process.cwd();
}

/**
 * Extracts the host command by stripping the HOST: prefix.
 * @param {string} cmd
 * @returns {string}
 */
export function stripHostPrefix(cmd) {
  const trimmed = cmd.trim();
  return trimmed.slice(5).trimStart();
}
