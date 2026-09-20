// @effect-diagnostics nodeBuiltinImport:off - Tests use disposable real Git repositories.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { validatePsiWorkspace } from "./PsiWorkspace.ts";
test("Psi refuses mutable Git config and host programs before command handoff", async () => {
  const root=mkdtempSync(join(tmpdir(),"psi-git-boundary-"));
  const git=(...args:string[])=>execFileSync("git",["-C",root,...args]);
  try {
    git("init","-q");
    await validatePsiWorkspace(root);
    git("config","filter.escape.clean","sh ./mutable.sh");
    await assert.rejects(validatePsiWorkspace(root),/external_program/);
    git("config","--unset","filter.escape.clean");
    git("config","include.path","../future-config");
    await assert.rejects(validatePsiWorkspace(root),/mutable_config/);
    writeFileSync(join(root,"future-config"),"[alias]\n harmless = status\n");
    await assert.rejects(validatePsiWorkspace(root),/mutable_config/);
  } finally {rmSync(root,{recursive:true,force:true});}
});
