#!/usr/bin/env bash
set -euo pipefail

# Test-only source staging. Scaffolding and production dependency policy are unchanged.
# Usage: prepare_generated_axum_container.sh /empty/output/parent
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PARENT="${1:?pass a new disposable output directory}"
[[ $# -eq 1 ]] || { echo 'Expected one output directory' >&2; exit 2; }
[[ ! -e "$PARENT" ]] || { echo 'Output directory must not exist' >&2; exit 2; }
mkdir -p "$PARENT"
PARENT="$(cd "$PARENT" && pwd)"
[[ "$PARENT/" != "$ROOT/"* ]] || { echo 'Fixture must be outside the source checkout' >&2; exit 2; }
RUSTUP_TOOLCHAIN="$(awk '$1 == "rust" { print $2 }' "$ROOT/.tool-versions")"
export RUSTUP_TOOLCHAIN
[[ "$RUSTUP_TOOLCHAIN" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Expected declared Rust pin' >&2; exit 2; }
cargo run --locked --manifest-path "$ROOT/Cargo.toml" -p edgezero-cli --bin edgezero -- \
  new container-probe --dir "$PARENT"
APP="$PARENT/container-probe"
python3 - "$ROOT" "$APP" <<'PY'
import hashlib, json, pathlib, re, shutil, subprocess, sys, tomllib
root, app = map(pathlib.Path, sys.argv[1:])
vendor = app / 'vendor' / 'edgezero'
paths = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=root).decode().split('\0')
source_hash = hashlib.sha256()
for name in sorted(set(filter(None, paths))):
    # Only the crate graph and its required optional example sources are needed.
    if name not in ('Cargo.toml', 'Cargo.lock', '.tool-versions') and not name.startswith(('crates/', 'examples/app-demo/')):
        continue
    source = root / name
    if not source.is_file() or source.is_symlink() or not source.resolve().is_relative_to(root):
        raise ValueError(f'Expected regular source file: {name}')
    destination = vendor / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    source_hash.update(name.encode() + b'\0' + source.read_bytes() + b'\0')
manifest = app / 'Cargo.toml'
text = manifest.read_text()
deps = tomllib.loads(text)['workspace']['dependencies']
for name, dep in deps.items():
    if not name.startswith('edgezero-'):
        continue
    if not isinstance(dep, dict) or 'path' not in dep or not (vendor / 'crates' / name / 'Cargo.toml').is_file():
        raise ValueError(f'Expected generated checkout dependency: {name}')
    pattern = rf'(?m)^({re.escape(name)}\s*=\s*\{{[^\n]*?\bpath\s*=\s*)"[^"]*"'
    text, count = re.subn(pattern, lambda m: m[1] + f'"vendor/edgezero/crates/{name}"', text)
    if count != 1:
        raise ValueError(f'Cannot stage generated dependency: {name}')
# Cargo otherwise auto-enrolls in-tree path dependencies into the app workspace,
# losing the staged framework's own workspace dependency/lint inheritance.
require_resolver = 'resolver = "2"'
if text.count(require_resolver) != 1:
    raise ValueError('Unexpected generated workspace resolver')
text = text.replace(require_resolver, 'exclude = ["vendor/edgezero"]\n' + require_resolver, 1)
manifest.write_text(text)
shutil.rmtree(app / '.git')
revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
(app / 'fixture-source.json').write_text(json.dumps({
    'revision': revision, 'source_sha256': source_hash.hexdigest(),
    'dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=root)),
    'dependency_mode': 'staged-test-sources',
}, indent=2) + '\n')
PY
(
  cd "$APP"
  cargo generate-lockfile
  cargo metadata --locked --format-version 1 > "$PARENT/metadata.json"
)
python3 - "$APP" "$PARENT/metadata.json" <<'PY'
import hashlib, json, pathlib, re, sys
app = pathlib.Path(sys.argv[1])
metadata = json.loads(pathlib.Path(sys.argv[2]).read_text())
for package in metadata['packages']:
    if package['source'] is None and not pathlib.Path(package['manifest_path']).resolve().is_relative_to(app):
        raise ValueError(f'Out-of-context dependency: {package["name"]}')
inputs = json.loads((app / 'fixture-source.json').read_text())
inputs['package'] = 'container-probe-adapter-axum'
inputs['binary'] = 'container-probe-adapter-axum'
inputs['lock_sha256'] = hashlib.sha256((app / 'Cargo.lock').read_bytes()).hexdigest()
inputs['toolchain'] = (app / '.tool-versions').read_text()
inputs['dockerfile_sha256'] = hashlib.sha256((app / 'Dockerfile').read_bytes()).hexdigest()
inputs['base_images'] = re.findall(r'^FROM (\S+)', (app / 'Dockerfile').read_text(), re.M)
context = hashlib.sha256()
for path in sorted(app.rglob('*'), key=lambda path: path.relative_to(app).as_posix()):
    if path.is_file():
        context.update(path.relative_to(app).as_posix().encode() + b'\0' + path.read_bytes() + b'\0')
inputs['context_sha256'] = context.hexdigest()
(app / 'fixture-inputs.json').write_text(json.dumps(inputs, indent=2) + '\n')
PY
printf 'Prepared disposable fixture: %s\n' "$APP"
