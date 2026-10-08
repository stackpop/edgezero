"""Opt-in checks of the actual generated/staged context, without rebuilding it."""
import hashlib
import json
import os
from pathlib import Path
import tomllib
import unittest

APP = os.environ.get('AXUM_PREPARED_CONTEXT')


@unittest.skipUnless(APP, 'set AXUM_PREPARED_CONTEXT to the freshly prepared fixture')
class PreparedContext(unittest.TestCase):
    def setUp(self):
        self.app = Path(APP).resolve()
        self.manifest = tomllib.loads((self.app / 'Cargo.toml').read_text())
        self.inputs = json.loads((self.app / 'fixture-inputs.json').read_text())

    def test_nested_workspace_and_features_preserved(self):
        self.assertEqual(self.manifest['workspace']['exclude'], ['vendor/edgezero'])
        for name, dep in self.manifest['workspace']['dependencies'].items():
            if name.startswith('edgezero-'):
                self.assertEqual(dep['path'], f'vendor/edgezero/crates/{name}')
        axum = tomllib.loads((self.app / 'crates/container-probe-adapter-axum/Cargo.toml').read_text())
        self.assertEqual(axum['dependencies']['edgezero-adapter-axum']['features'], ['axum'])
        cli = tomllib.loads((self.app / 'crates/container-probe-cli/Cargo.toml').read_text())
        self.assertNotIn('default-features', cli['dependencies']['edgezero-cli'])
        framework = tomllib.loads((self.app / 'vendor/edgezero/Cargo.toml').read_text())
        self.assertIn('async-compression', framework['workspace']['dependencies'])

    def test_own_lockfile_and_confined_local_packages(self):
        self.assertFalse((self.app / '.git').exists())
        self.assertEqual(hashlib.sha256((self.app / 'Cargo.lock').read_bytes()).hexdigest(), self.inputs['lock_sha256'])
        self.assertNotEqual((self.app / 'Cargo.lock').read_bytes(), (self.app / 'vendor/edgezero/Cargo.lock').read_bytes())
        metadata = json.loads((self.app.parent / 'metadata.json').read_text())
        for package in metadata['packages']:
            if package['source'] is None:
                self.assertTrue(Path(package['manifest_path']).resolve().is_relative_to(self.app), package['name'])

    def test_no_git_or_host_build_output_staged(self):
        for path in (self.app / 'vendor').rglob('*'):
            relative = path.relative_to(self.app)
            self.assertNotIn('.git', relative.parts)
            self.assertNotIn('target', relative.parts)
        self.assertEqual(self.inputs['dependency_mode'], 'staged-test-sources')


if __name__ == '__main__':
    unittest.main()
