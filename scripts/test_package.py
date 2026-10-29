import hashlib
from pathlib import Path
import tarfile
import tempfile
import unittest
import zipfile

import package


class PackageTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.binary = self.root / 'built'
        self.binary.write_bytes(b'binary')

    def tearDown(self):
        self.temporary.cleanup()

    def test_a_unix_target_gets_versioned_and_alias_tarballs_rooted_under_the_repo_name(self):
        paths = package.package(self.binary, 'x86_64-unknown-linux-gnu', self.root / 'dist')
        version = package.version()
        self.assertEqual(
            [path.name for path in paths],
            [f'powermetrics-plugin-v{version}-x86_64-unknown-linux-gnu.tar.gz',
             'powermetrics-plugin-x86_64-unknown-linux-gnu.tar.gz'])
        with tarfile.open(paths[1]) as archive:
            names = set(archive.getnames())
            executable = archive.getmember('powermetrics-plugin/powermetrics-plugin')
        self.assertTrue(executable.mode & 0o111, 'the executable keeps its execute bit')
        self.assertTrue({'powermetrics-plugin/plugin.toml', 'powermetrics-plugin/README.md',
                         'powermetrics-plugin/LICENSE', 'powermetrics-plugin/plugin-manifest.json',
                         'powermetrics-plugin/bundle/energy.js'} <= names)

    def test_a_windows_target_gets_zips_with_an_exe(self):
        paths = package.package(self.binary, 'x86_64-pc-windows-msvc', self.root / 'dist')
        self.assertTrue(all(path.suffix == '.zip' for path in paths))
        with zipfile.ZipFile(paths[0]) as archive:
            names = set(archive.namelist())
        self.assertIn('powermetrics-plugin/powermetrics-plugin.exe', names)
        self.assertIn('powermetrics-plugin/plugin.toml', names)
        self.assertIn('powermetrics-plugin/plugin-manifest.json', names)
        self.assertIn('powermetrics-plugin/bundle/energy.js', names)

    def test_each_archive_has_a_matching_checksum_file(self):
        for path in package.package(self.binary, 'aarch64-apple-darwin', self.root / 'dist'):
            digest, name = path.with_name(path.name + '.sha256').read_text().split()
            self.assertEqual(name, path.name)
            self.assertEqual(digest, hashlib.sha256(path.read_bytes()).hexdigest())


if __name__ == '__main__':
    unittest.main()
