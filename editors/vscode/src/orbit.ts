// What the extension asks the `orbit` and `orbit-lsp` programs, kept apart
// from the VS Code API so it can be tested with plain Node.

import { execFile } from "node:child_process";

/** A program a debugger can attach to, as `orbit --debug-attach --list` describes it. */
export interface Attachable {
  pid: number;
  program: string | null;
  args: string[];
  /** Serving already; otherwise it serves when signalled (Linux). */
  listening: boolean;
}

/** The programs in `orbit --debug-attach --list`'s output (a JSON array). */
export function parseList(output: string): Attachable[] {
  const list: unknown = JSON.parse(output);
  if (!Array.isArray(list)) {
    throw new Error("expected a JSON array");
  }
  return list
    .filter((e): e is Record<string, unknown> => typeof e === "object" && e !== null && typeof e.pid === "number")
    .map((e) => ({
      pid: e.pid as number,
      program: typeof e.program === "string" ? e.program : null,
      args: Array.isArray(e.args) ? e.args.filter((a): a is string => typeof a === "string") : [],
      listening: e.listening === true,
    }));
}

/** One line for a picker: the script and its arguments, or that it has none yet. */
export function describe(a: Attachable): string {
  const script = a.program ?? "(no script)";
  return [script, ...a.args].join(" ");
}

/** Run `orbit --debug-attach --list`. */
export function listAttachable(orbit: string): Promise<Attachable[]> {
  return new Promise((resolve, reject) => {
    execFile(orbit, ["--debug-attach", "--list"], { timeout: 10_000 }, (err, stdout, stderr) => {
      if (err) {
        reject(new Error(stderr.trim() || err.message));
        return;
      }
      try {
        resolve(parseList(stdout));
      } catch (e) {
        reject(new Error(`unexpected output from ${orbit} --debug-attach --list: ${(e as Error).message}`));
      }
    });
  });
}

/** The command line of the debug adapter for a `launch` or an `attach` to `pid`. */
export function adapterArgs(request: string, pid?: number | string): string[] {
  if (request === "attach") {
    const n = typeof pid === "string" ? Number.parseInt(pid, 10) : pid;
    if (n === undefined || !Number.isInteger(n) || n <= 0) {
      throw new Error("an attach configuration needs a process id ('pid')");
    }
    return ["--debug-attach", String(n)];
  }
  return ["--debug-server"];
}

/** `initializationOptions` for orbit-lsp. */
export function serverOptions(searchPath: readonly string[]): { searchPath: string[] } {
  return { searchPath: searchPath.filter((p) => p.trim() !== "") };
}
