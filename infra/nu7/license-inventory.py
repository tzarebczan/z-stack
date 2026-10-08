"""Retain exact runtime/build dependency notices for the patched Zaino binary."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

root = Path(os.environ.get('SOURCE_ROOT', '/src'))
output = Path(os.environ.get('OUTPUT_DIR', '/out'))
supplemental = Path(os.environ.get('SUPPLEMENTAL_LICENSES', '/supplemental'))
origins = json.loads((supplemental / 'sources.json').read_text())
tree = subprocess.check_output(['cargo', 'tree', '--locked', '--offline', '-p', 'zainod',
    '--features', 'no_tls_use_unencrypted_traffic', '--target', 'x86_64-unknown-linux-gnu',
    '--edges', 'normal,build', '--prefix', 'none', '--format', '{p}'], cwd=root, text=True)
selected = set(re.findall(r'^(\S+) v(\S+)', tree, re.M))
paths = {(n, v): Path(p) for n, v, p in re.findall(r'^(\S+) v(\S+) (?:\(proc-macro\) )?\(([^)]+)\)', tree, re.M) if Path(p).is_absolute()}
registry = Path(os.environ.get('CARGO_HOME', '/usr/local/cargo')) / 'registry/src'
records, texts = [], ['Patched Zaino: selected runtime and build dependencies.\nOriginal upstream notices are retained; inclusion does not imply all code is shipped.\n']
for name, version in sorted(selected):
    candidates = [paths[(name, version)]] if (name, version) in paths else list(registry.glob(f'*/{name}-{version}'))
    assert len(candidates) == 1, f'missing/ambiguous source: {name} {version}'
    directory = candidates[0]
    manifest = tomllib.loads((directory / 'Cargo.toml').read_text())['package']
    license_id = manifest.get('license')
    if isinstance(license_id, dict):
        license_id = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['package']['license']
    assert license_id, f'missing declaration: {name}'
    files = sorted(p for p in directory.iterdir() if p.is_file() and re.match(r'^(LICENSE|COPYING|COPYRIGHT|NOTICE)', p.name, re.I))
    files += sorted(p for d in directory.iterdir() if d.is_dir() and d.name.lower() == 'licenses' for p in d.rglob('*') if p.is_file())
    if not files and directory.is_relative_to(root):
        files = [root / 'LICENSE']
    retained = sorted(supplemental.glob(name + '-' + version + '-*'))
    if not files:
        files = retained or sorted(supplemental.glob(name + '-*'))
    if retained:
        files += [p for p in retained if p not in files]
    # C/C++ dependencies retain notices inside their vendored source trees too.
    files += sorted(p for p in directory.rglob('*') if p.is_file() and p.parent != directory and re.match(r'^(LICENSE|COPYING|COPYRIGHT|NOTICE)(?:[._-]|$)', p.name, re.I) and p not in files)
    assert files, f'retain missing upstream license texts: {name} {version}'
    record = {'name': name, 'version': version, 'license': license_id, 'texts': []}
    texts.append(f'\n{"=" * 72}\n{name} {version} — {license_id}\n')
    for path in files:
        data = path.read_bytes()
        origin = origins[path.name]['url'] if path.parent == supplemental else (
            'https://github.com/zingolabs/zaino/blob/3244a74bb09fa6a09a4b2deeb6be53bab0890747/' + str(path.relative_to(root))
            if path.is_relative_to(root) else f'https://docs.rs/crate/{name}/{version}/source/{path.relative_to(directory)}')
        record['texts'].append({'file': path.name, 'sha256': hashlib.sha256(data).hexdigest(), 'origin': origin})
        if path.parent == supplemental:
            assert hashlib.sha256(data).hexdigest() == origins[path.name]['sha256'], f'changed retained notice: {path.name}'
            if origins[path.name].get('note'):
                record['texts'][-1]['note'] = origins[path.name]['note']
                texts.append(origins[path.name]['note'] + '\n')
        texts.append(f'\n--- {path.name} ---\n' + data.decode('utf-8') + '\n')
    records.append(record)
(output / 'THIRD_PARTY_LICENSES.txt').write_text(''.join(texts))
(output / 'dependencies.json').write_text(json.dumps({'target': 'x86_64-unknown-linux-gnu', 'edges': 'normal,build', 'cargoLockSha256': hashlib.sha256((root / 'Cargo.lock').read_bytes()).hexdigest(), 'dependencies': records}, indent=2) + '\n')
print(f'Retained notices for {len(records)} dependencies')
