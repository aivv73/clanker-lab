#!/usr/bin/env python3
"""Build the pinned, checked Psi policy CPU executable and its evidence manifest."""
import argparse, hashlib, json, os, pathlib, platform, shutil, subprocess, time
HERE = pathlib.Path(__file__).resolve().parent
REV = '15ae0c86f3193b8f645b4bedbc438655b648d0da'
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--bend-source', type=pathlib.Path)
args = parser.parse_args()
build = HERE / '.build'
build.mkdir(exist_ok=True)
source = args.bend_source.resolve() if args.bend_source else build / 'bend'
env = dict(os.environ, BEND_NO_TELEMETRY='1')
def run(argv, timeout=120):
    return subprocess.run([str(x) for x in argv], capture_output=True, text=True, env=env, timeout=timeout)
def checked(argv, timeout=120):
    r = run(argv, timeout)
    if r.returncode:
        raise SystemExit(f'Command failed: {argv}\n{r.stdout}\n{r.stderr}')
    return r
if not source.exists():
    source.mkdir()
    checked(['git','init',source])
    checked(['git','-C',source,'fetch','--depth','1','https://github.com/bendlang/bend.git',REV])
    checked(['git','-C',source,'checkout','--detach','FETCH_HEAD'])
if checked(['git','-C',source,'rev-parse','HEAD']).stdout.strip() != REV:
    raise SystemExit('Compiler checkout does not match pinned revision')
if checked(['git','-C',source,'status','--porcelain','--untracked-files=no']).stdout.strip():
    raise SystemExit('Compiler checkout has modified tracked files')
compiler = ['bun',source/'bend2/main.ts']
# The complete proof dependency graph is these three local modules. No Base/IO,
# external import, unsafe block, or unimplemented law is permitted in this graph.
expected_imports = {'Policy.bend': [], 'LAWS.bend': ['import ./Policy.bend as Policy'], 'PROOF.bend': ['import ./LAWS.bend as Laws']}
import re
for name, imports in expected_imports.items():
    text = (HERE/name).read_text()
    if re.search(r'\b(unsafe|assume|axiom|sorry)\b', text):
        raise SystemExit('Untrusted proof construct: '+name)
    if [line for line in text.splitlines() if line.startswith('import ')] != imports:
        raise SystemExit('Proof dependency graph changed: '+name)
names = ['Policy.bend', 'LAWS.bend', 'PROOF.bend', 'Main.bend']
hashes = {name: hashlib.sha256((HERE/name).read_bytes()).hexdigest() for name in names}
proof = checked([*compiler,HERE/'PROOF.bend'])
if proof.stdout.strip() != 'All terms check.' or proof.stderr.strip():
    raise SystemExit('Strict proof gate failed: '+proof.stdout+proof.stderr)
mutant = build/'mutant'; mutant.mkdir(exist_ok=True)
for name in ['Policy.bend','LAWS.bend','PROOF.bend']:
    shutil.copyfile(HERE/name,mutant/name)
p=mutant/'Policy.bend'; original=p.read_text()
old='case Cancelled{}: Result{Cancelled{}, Deny{}}'
if original.count(old)!=1: raise SystemExit('Mutation anchor changed')
p.write_text(original.replace(old,'case Cancelled{}: Result{Cancelled{}, Permit{}}'))
negative=run([*compiler,mutant/'PROOF.bend'])
if negative.returncode==0 or not all(marker in (negative.stdout+negative.stderr) for marker in ['expected : Policy.Result{Policy.Cancelled{}, Policy.Permit{}}', 'observed : Policy.Result{Policy.Cancelled{}, Policy.Deny{}}', 'LAWS.cancelled_step']):
    # A parse error, missing executable or timeout is not evidence of rejecting a false law.
    raise SystemExit('Expected a proof type mismatch for deliberately broken policy: '+negative.stdout+negative.stderr)
compiled=checked([*compiler,HERE/'Main.bend','-o',build/'policy'])

if hashes != {name: hashlib.sha256((HERE/name).read_bytes()).hexdigest() for name in names}:
    raise SystemExit('Policy sources changed during build')
report = {
    'compiler_commit': REV,
    'compiler_tree': checked(['git','-C',source,'rev-parse','HEAD^{tree}']).stdout.strip(),
    'compiler_entrypoint_sha256': hashlib.sha256((source/'bend2/main.ts').read_bytes()).hexdigest(),
    'bun_binary_sha256': hashlib.sha256(pathlib.Path(shutil.which('bun')).read_bytes()).hexdigest(),
    'compiler_version': checked([*compiler, '--version']).stdout.strip(),
    'bun': checked(['bun','--version']).stdout.strip(),
    'clang': checked(['clang','--version']).stdout.splitlines()[0],
    'os': platform.platform(),
    'proof_stdout': proof.stdout,
    'proof_stderr': proof.stderr,
    'mutation_exit': negative.returncode,
    'mutation_output': negative.stdout + negative.stderr,
    'source_sha256': hashes,
    'binary_sha256': hashlib.sha256((build/'policy').read_bytes()).hexdigest(),
    'proof_dependencies': expected_imports,
    'recorded_utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
}
(build/'manifest.json').write_text(json.dumps(report, indent=2)+'\n')
print('Checked policy and rejected post-cancel Permit mutant. Bundle:', build)
