// @effect-diagnostics nodeBuiltinImport:off - Validate the host Git boundary before sandbox handoff.
import { execFile } from "node:child_process";
import { lstatSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { homedir } from "node:os";
import { promisify } from "node:util";
const git = promisify(execFile);
const within = (parent: string, child: string) => {
  const rel = relative(parent, child);
  return !rel || (!isAbsolute(rel) && rel !== ".." && !rel.startsWith("../"));
};
function canonical(path: string): string {
  try { return realpathSync(path); } catch {
    const parent = dirname(path);
    if (parent === path) throw new Error("psi_git_config_unreadable");
    return resolve(canonical(parent), relative(parent, path));
  }
}
/** Git runs in T3, outside the command sandbox. Mutable includes, filters and
 * external diff/fsmonitor programs must not turn a file change into host execution.
 * The executor mounts the selected workspace's .git metadata read-only. */
export async function validatePsiWorkspace(cwd: string) {
  const workspace = realpathSync(cwd);
  const metadata = resolve(workspace, ".git");
  const metadataStat = lstatSync(metadata);
  if (metadataStat.isSymbolicLink() || (metadataStat.isFile() && metadataStat.nlink !== 1)) throw new Error("psi_git_symlink_or_hardlink_unsupported");
  const protectedMetadata = lstatSync(metadata).isDirectory() ? realpathSync(metadata) : undefined;
  const { stdout } = await git("git", ["-C",workspace,"config","--null","--show-origin","--list"], {timeout: 2000, maxBuffer: 1024*1024});
  const entries = stdout.split("\0");
  for (let i=0;i+1<entries.length;i+=2) {
    const origin = entries[i]!;
    const entry = entries[i+1]!;
    const split = entry.indexOf("\n");
    const key = (split < 0 ? entry : entry.slice(0,split)).toLowerCase();
    const value = split < 0 ? "" : entry.slice(split+1);
    if (/^filter\..+\.(clean|smudge|process)$/.test(key)
      || /^diff\..+\.(command|textconv)$/.test(key) || key === "diff.external"
      || (key === "core.fsmonitor" && value !== "false")) {
      throw new Error("psi_git_external_program_unsupported");
    }
    if (!origin.startsWith("file:")) continue;
    const source = canonical(resolve(workspace,origin.slice(5)));
    const mutable = (path: string) => within(workspace,path) && !(protectedMetadata && within(protectedMetadata,path));
    if (mutable(source) || lstatSync(source).nlink !== 1) throw new Error("psi_git_mutable_config_unsupported");
    if (/^(include|includeif\..+)\.path$/.test(key)) {
      const target = value.startsWith("~/") ? resolve(homedir(),value.slice(2)) : resolve(dirname(source),value);
      if (mutable(canonical(target))) throw new Error("psi_git_mutable_config_unsupported");
    }
    if (key === "core.hookspath" && mutable(canonical(resolve(workspace,value)))) {
      throw new Error("psi_git_mutable_hooks_unsupported");
    }
  }
}
