#!/usr/bin/env python3
"""Package a built powermetrics-plugin binary as the release archives
`mesh-llm plugins install Mesh-LLM/powermetrics-plugin` downloads.

Mesh resolves a GitHub reference by repository name, so the archive is rooted
under `powermetrics-plugin/` and holds `plugin.toml` and an executable of that
name, plus the console page: `plugin-manifest.json` declares it and `bundle/`
holds its script. Each archive is written twice, under the versioned name and the stable
alias the installer falls back to, each with a `.sha256` file.
"""
import argparse
import hashlib
from pathlib import Path
import re
import shutil
import tarfile
import tempfile
import zipfile

NAME = 'powermetrics-plugin'
ROOT = Path(__file__).resolve().parent.parent


def version():
    cargo = re.search(r'^version = "(.+)"$', (ROOT / 'Cargo.toml').read_text(), re.M).group(1)
    plugin = re.search(r'^version = "(.+)"$', (ROOT / 'plugin.toml').read_text(), re.M).group(1)
    if cargo != plugin:
        raise SystemExit(f'Cargo.toml says {cargo} but plugin.toml says {plugin}')
    return cargo


def windows(target):
    return target.endswith('-windows-msvc')


def write_checksum(path):
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    path.with_name(path.name + '.sha256').write_text(f'{digest}  {path.name}\n')


def package(binary, target, output):
    output.mkdir(parents=True, exist_ok=True)
    extension = 'zip' if windows(target) else 'tar.gz'
    executable = NAME + ('.exe' if windows(target) else '')
    names = [f'{NAME}-v{version()}-{target}.{extension}', f'{NAME}-{target}.{extension}']
    with tempfile.TemporaryDirectory(prefix='powermetrics-package-') as temporary:
        stage = Path(temporary) / NAME
        stage.mkdir()
        shutil.copy2(binary, stage / executable)
        for name in ('plugin.toml', 'plugin-manifest.json', 'README.md', 'LICENSE'):
            shutil.copy2(ROOT / name, stage / name)
        shutil.copytree(ROOT / 'bundle', stage / 'bundle')
        first = output / names[0]
        if windows(target):
            with zipfile.ZipFile(first, 'w', zipfile.ZIP_DEFLATED) as archive:
                for path in sorted(stage.rglob('*')):
                    archive.write(path, path.relative_to(stage.parent).as_posix())
        else:
            # Set the modes here, so an archive built on any host installs runnable.
            def mode(info):
                executable_file = info.name == f'{NAME}/{executable}'
                info.mode = 0o755 if info.isdir() or executable_file else 0o644
                return info

            with tarfile.open(first, 'w:gz') as archive:
                archive.add(stage, arcname=NAME, filter=mode)
    shutil.copy2(first, output / names[1])
    for name in names:
        write_checksum(output / name)
    return [output / name for name in names]


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('target')
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    arguments = parser.parse_args()
    for path in package(arguments.binary, arguments.target, arguments.output):
        print(path)
