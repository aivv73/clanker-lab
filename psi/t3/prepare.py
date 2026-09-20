#!/usr/bin/env python3
"""Install Psi's small overlay into a disposable, pinned T3 checkout."""
import pathlib
import shutil
import subprocess
import sys

PIN = "dfbb11bdd7c3f1a5575cb55d3e3abb12be025727"
HERE = pathlib.Path(__file__).resolve().parent


def prepare(checkout):
    checkout = pathlib.Path(checkout).resolve()
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=checkout, text=True).strip()
    if revision != PIN:
        raise SystemExit(f"T3 revision must be {PIN}, got {revision}")
    patch = str(HERE / "t3.patch")
    result = subprocess.run(["git", "apply", "--check", patch], cwd=checkout, capture_output=True)
    if result.returncode == 0:
        subprocess.run(["git", "apply", patch], cwd=checkout, check=True)
    else:
        subprocess.run(["git", "apply", "--reverse", "--check", patch], cwd=checkout, check=True)
    target = checkout / "apps/server/src/psi"
    target.mkdir(exist_ok=True)
    for name in ("PsiDriver.ts", "PsiBridge.ts", "PsiInbox.ts", "PsiProjection.ts", "inbox.test.ts", "durability-worker.ts", "adapter.test.ts"):
        if (HERE / name).exists():
            shutil.copyfile(HERE / name, target / name)
    (checkout / "tsconfig.psi.json").write_text('{"extends":"./tsconfig.base.json","compilerOptions":{"noEmit":true,"skipLibCheck":true,"types":["node"]},"include":["apps/server/src/psi/*.ts","apps/server/src/orchestration/Layers/ProviderRuntimeIngestion.ts"]}\n')
    return checkout


if __name__ == "__main__":
    prepare(sys.argv[1])
