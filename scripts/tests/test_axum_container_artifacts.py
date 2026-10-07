import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock
import urllib.error

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('verify', ROOT / 'examples/container-publishing/verify_artifacts.py')
v = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(v)
REV = 'a' * 40


def put_json(path, value):
    path.write_text(json.dumps(value))


def fixture(root, arch='amd64', revision=REV):
    layout = root / 'oci'
    (layout / 'blobs/sha256').mkdir(parents=True)

    def store(value, media_type):
        data = json.dumps(value).encode() if isinstance(value, dict) else value
        digest = hashlib.sha256(data).hexdigest()
        (layout / 'blobs/sha256' / digest).write_bytes(data)
        return {'mediaType': media_type, 'size': len(data), 'digest': 'sha256:' + digest}

    raw_layer = io.BytesIO()
    with tarfile.open(fileobj=raw_layer, mode='w') as tar:
        entry = tarfile.TarInfo('transport-fixture')
        entry.size = 5
        tar.addfile(entry, io.BytesIO(b'layer'))
    diff_id = 'sha256:' + hashlib.sha256(raw_layer.getvalue()).hexdigest()
    config = store({'os': 'linux', 'architecture': arch,
                    'rootfs': {'type': 'layers', 'diff_ids': [diff_id]},
                    'config': {'Labels': {'org.opencontainers.image.revision': revision}}}, v.CONFIG)
    layer = store(gzip.compress(raw_layer.getvalue(), mtime=0), 'application/vnd.oci.image.layer.v1.tar+gzip')
    image = store({'schemaVersion': 2, 'mediaType': v.MANIFEST, 'config': config, 'layers': [layer]}, v.MANIFEST)
    put_json(layout / 'oci-layout', {'imageLayoutVersion': '1.0.0'})
    put_json(layout / 'index.json', {'schemaVersion': 2, 'mediaType': v.INDEX, 'manifests': [image]})
    with tarfile.open(root / 'image.tar', 'w') as archive:
        for path in sorted(layout.rglob('*')):
            archive.add(path, arcname=path.relative_to(layout).as_posix(), recursive=False)
    put_json(root / 'inputs.json', {'revision': revision, 'dirty': False, 'toolchain': 'rust 1.95.0\n',
                                   'source_sha256': 'a' * 64, 'context_sha256': 'b' * 64,
                                   'lock_sha256': 'c' * 64, 'dockerfile_sha256': 'd' * 64,
                                   'base_images': ['debian@sha256:' + 'e' * 64]})
    put_json(root / 'scan.json', {'SchemaVersion': 2, 'Metadata': {'ImageID': config['digest'],
                                 'DiffIDs': [diff_id], 'OS': {'Family': 'debian'}},
                                 'Results': [{'Class': 'os-pkgs', 'Type': 'debian', 'Target': 'fixture', 'Vulnerabilities': []}]})
    put_json(root / 'scanner.json', {'Version': 'test', 'VulnerabilityDB': {'UpdatedAt': '2026-10-07'},
                                    'SubjectManifestDigest': image['digest']})
    put_json(root / 'sbom.json', {'bomFormat': 'CycloneDX', 'specVersion': '1.6', 'version': 1,
                                 'components': [{'type': 'library', 'name': 'transport-fixture'}],
                                 'metadata': {'component': {'type': 'container', 'name': 'fixture', 'properties': [
                                     {'name': 'aquasecurity:trivy:ImageID', 'value': config['digest']} ]}}})
    put_json(root / 'provenance.json', {'revision': revision, 'subject': image['digest'],
                                       'run_id': '123-1', 'inputs': v.load(root / 'inputs.json')})
    return image


class Artifacts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.image = fixture(self.root)

    def seal(self):
        return v.seal(self.root, REV, '123-1', 'linux/amd64')

    def verify(self, revision=REV, run_id='123-1', platform='linux/amd64'):
        return v.verify(self.root, revision, run_id, platform, [])

    def test_receipt_and_archive_roundtrip(self):
        receipt = self.seal()
        self.assertEqual(self.verify(), receipt)
        v.unpack(self.root / 'image.tar', self.root / 'copy')
        self.assertEqual(v.describe(self.root / 'copy', 'linux/amd64')['manifest_digest'], self.image['digest'])

    def test_arm64(self):
        other = self.root / 'arm'
        other.mkdir()
        fixture(other, 'arm64')
        v.seal(other, REV, '123-1', 'linux/arm64')
        v.verify(other, REV, '123-1', 'linux/arm64', [])

    def test_wrong_run_source_platform_and_acceptance(self):
        receipt = self.seal()
        for kwargs in ({'revision': 'b' * 40}, {'run_id': '124-1'}, {'platform': 'linux/arm64'}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                self.verify(**kwargs)
        receipt['checks'] = 'failed'
        put_json(self.root / 'receipt.json', receipt)
        with self.assertRaises(ValueError):
            self.verify()

    def test_mutated_archive_or_sidecar(self):
        self.seal()
        with open(self.root / 'image.tar', 'ab') as stream:
            stream.write(b'changed')
        with self.assertRaises(ValueError):
            self.verify()
        self.seal()
        put_json(self.root / 'sbom.json', {'bomFormat': 'changed'})
        with self.assertRaises(ValueError):
            self.verify()

    def test_empty_scan_wrong_subject_and_dirty_source(self):
        for changes in ({'Results': []}, {'Metadata': {'ImageID': 'sha256:' + 'f' * 64}}):
            report = v.load(self.root / 'scan.json')
            report.update(changes)
            put_json(self.root / 'scan.json', report)
            self.seal()
            with self.assertRaises(ValueError):
                self.verify()
        inputs = v.load(self.root / 'inputs.json')
        inputs['dirty'] = True
        put_json(self.root / 'inputs.json', inputs)
        with self.assertRaises(ValueError):
            self.seal()

    def test_pulled_identity_on_both_docker_stores(self):
        info = v.describe(self.root / 'oci', 'linux/amd64')
        config = v.load(self.root / 'oci/blobs/sha256' / info['config_digest'][7:])
        reference = '127.0.0.1/app@' + info['manifest_digest']
        inspection = {'Id': info['manifest_digest'], 'RepoDigests': [reference],
                      'Os': 'linux', 'Architecture': 'amd64', 'Config': config['config'],
                      'RootFS': {'Layers': config['rootfs']['diff_ids']},
                      'Descriptor': {'digest': info['manifest_digest']}}
        v.verify_pulled(self.root / 'oci', [inspection], 'linux/amd64', reference)
        inspection['Id'] = info['config_digest']
        inspection.pop('Descriptor')
        v.verify_pulled(self.root / 'oci', [inspection], 'linux/amd64', reference)
        inspection['RootFS']['Layers'] = []
        with self.assertRaises(ValueError):
            v.verify_pulled(self.root / 'oci', [inspection], 'linux/amd64', reference)

    def test_invalid_base_pins_and_symlink_sidecars(self):
        original = v.load(self.root / 'inputs.json')
        for bases in ('not-a-list', [], ['debian@sha256:garbage'], [5]):
            with self.assertRaises(ValueError):
                v.check_inputs(dict(original, base_images=bases), REV, '123-1')
        self.seal()
        (self.root / 'sbom.json').rename(self.root / 'other.json')
        (self.root / 'sbom.json').symlink_to(self.root / 'other.json')
        with self.assertRaises(ValueError):
            self.verify()

    def test_sbom_wrong_subject_and_scan_wrong_filesystem(self):
        self.seal()
        bill = v.load(self.root / 'sbom.json')
        bill['metadata']['component']['properties'][0]['value'] = 'sha256:' + 'f' * 64
        put_json(self.root / 'sbom.json', bill)
        self.seal()
        with self.assertRaises(ValueError):
            self.verify()
        report = v.load(self.root / 'scan.json')
        report['Metadata']['DiffIDs'] = []
        put_json(self.root / 'scan.json', report)
        self.seal()
        with self.assertRaises(ValueError):
            self.verify()

    def test_index_cli_rejects_unverified_receipts_and_mutated_bundles(self):
        self.seal()
        arm = self.root / 'arm'
        arm.mkdir()
        fixture(arm, 'arm64')
        v.seal(arm, REV, '123-1', 'linux/arm64')
        command = ['python3', str(SPEC.origin), 'index', str(self.root), str(arm), '--revision', REV, '--run-id', '123-1']
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(json.loads(result.stdout)['manifests']), 2)
        put_json(arm / 'scan.json', {})
        self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)

    def test_artifact_selection_binds_attempt_source_and_digest(self):
        metadata = {'artifacts': [dict(id=i, name=f'accepted-{arch}-123-1', expired=False,
                    digest='sha256:' + str(i) * 64, workflow_run={'id': 123, 'head_sha': REV})
                    for i, arch in enumerate(('amd64', 'arm64'), start=1)]}
        self.assertEqual(v.artifact_ids(metadata, '123', '1', REV), '1,2')
        for run, attempt, revision in (('124', '1', REV), ('123', '2', REV), ('123', '1', 'b' * 40)):
            with self.assertRaises(ValueError):
                v.artifact_ids(metadata, run, attempt, revision)
        metadata['artifacts'][0]['digest'] = None
        with self.assertRaises((ValueError, TypeError)):
            v.artifact_ids(metadata, '123', '1', REV)

    def test_registry_tag_preflight_fails_closed(self):
        expected = 'sha256:' + hashlib.sha256(b'index').hexdigest()
        for status, body, succeeds in ((404, {'errors': [{'code': 'MANIFEST_UNKNOWN'}]}, True),
                                       (404, {'errors': [{'code': 'UNAUTHORIZED'}]}, False),
                                       (401, {}, False), (500, {}, False)):
            error = urllib.error.HTTPError('http://registry', status, 'test', {}, io.BytesIO(json.dumps(body).encode()))
            with mock.patch.object(v.urllib.request, 'build_opener') as factory:
                factory.return_value.open.side_effect = error
                if succeeds:
                    self.assertEqual(v.check_tag('127.0.0.1:5000/app', 'fixture', expected)['state'], 'absent')
                else:
                    with self.assertRaises(ValueError):
                        v.check_tag('127.0.0.1:5000/app', 'fixture', expected)
        with mock.patch.object(v.urllib.request, 'build_opener') as factory:
            factory.return_value.open.return_value.__enter__.return_value.read.return_value = b'index'
            self.assertEqual(v.check_tag('127.0.0.1:5000/app', 'fixture', expected)['state'], 'same-index')
            with self.assertRaises(ValueError):
                v.check_tag('127.0.0.1:5000/app', 'fixture', 'sha256:' + 'f' * 64)

    def test_release_record_recomputes_committed_source(self):
        source = self.root / 'source'
        source.mkdir()
        for path, text in {'Cargo.lock': '# locked\n', '.tool-versions': 'rust 1.95.0\n',
                           'Dockerfile': 'FROM debian@sha256:' + 'e' * 64 + '\n',
                           'a/b': 'nested', 'a-extra/c': 'ordering'}.items():
            target = source / path
            target.parent.mkdir(exist_ok=True)
            target.write_text(text)
        def git(*args):
            return subprocess.check_output(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', *args], cwd=source, stderr=subprocess.DEVNULL)
        git('init', '-q')
        git('add', '.')
        git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', 'commit', '-qm', 'fixture')
        revision = git('rev-parse', 'HEAD').decode().strip()
        archive = git('archive', '--format=tar', revision)
        context = hashlib.sha256()
        for path in sorted(source.rglob('*'), key=lambda p: p.relative_to(source).as_posix()):
            if path.is_file() and '.git' not in path.relative_to(source).parts:
                context.update(path.relative_to(source).as_posix().encode() + b'\0' + path.read_bytes() + b'\0')
        bundles = []
        for arch in ('amd64', 'arm64'):
            bundle = self.root / ('release-' + arch)
            bundle.mkdir()
            fixture(bundle, arch, revision)
            inputs = v.load(bundle / 'inputs.json')
            inputs.update(source_sha256=hashlib.sha256(archive).hexdigest(), context_sha256=context.hexdigest(),
                          lock_sha256=v.checksum(source / 'Cargo.lock'), dockerfile_sha256=v.checksum(source / 'Dockerfile'))
            put_json(bundle / 'inputs.json', inputs)
            provenance = v.load(bundle / 'provenance.json')
            provenance['inputs'] = inputs
            put_json(bundle / 'provenance.json', provenance)
            v.seal(bundle, revision, '123-1', 'linux/' + arch)
            bundles.append(bundle)
        metadata = {'artifacts': [dict(id=i, name=f'accepted-{arch}-123-1', expired=False,
                    digest='sha256:' + str(i) * 64, workflow_run={'id': 123, 'head_sha': revision})
                    for i, arch in enumerate(('amd64', 'arm64'), start=1)]}
        record = v.release_record(source, bundles, metadata, revision, '123-1', [])
        self.assertEqual(record['source']['lock_sha256'], v.checksum(source / 'Cargo.lock'))
        self.assertEqual(len(record['children']), 2)
        inputs = v.load(bundles[0] / 'inputs.json')
        inputs['lock_sha256'] = 'f' * 64
        put_json(bundles[0] / 'inputs.json', inputs)
        provenance = v.load(bundles[0] / 'provenance.json')
        provenance['inputs'] = inputs
        put_json(bundles[0] / 'provenance.json', provenance)
        v.seal(bundles[0], revision, '123-1', 'linux/amd64')
        with self.assertRaises(ValueError):
            v.release_record(source, bundles, metadata, revision, '123-1', [])

    def test_blob_digest_and_size(self):
        path = v.blob(self.root / 'oci', self.image, {v.MANIFEST})
        path.write_bytes(b'changed')
        with self.assertRaises(ValueError):
            v.describe(self.root / 'oci', 'linux/amd64')

    def test_no_attestations_in_runtime_index(self):
        put_json(self.root / 'oci/index.json', {'schemaVersion': 2, 'mediaType': v.INDEX, 'manifests': [self.image, self.image]})
        with self.assertRaises(ValueError):
            v.describe(self.root / 'oci', 'linux/amd64')

    def test_config_platform_is_authority(self):
        with self.assertRaises(ValueError):
            v.describe(self.root / 'oci', 'linux/arm64')

    def test_provenance_subject(self):
        put_json(self.root / 'provenance.json', {'revision': REV, 'subject': 'sha256:' + 'b' * 64})
        with self.assertRaises(ValueError):
            self.seal()

    def test_index_requires_two_same_run_variants(self):
        one = self.seal()
        two = dict(one, platform='linux/arm64', manifest_digest='sha256:' + 'c' * 64)
        index = v.runtime_index([one, two])
        self.assertEqual({m['platform']['architecture'] for m in index['manifests']}, {'amd64', 'arm64'})
        for receipts in ([one], [one, one], [one, dict(two, run_id='124-1')],
                         [one, dict(two, sidecars={**two['sidecars'], 'inputs.json': 'f' * 64})]):
            with self.assertRaises(ValueError):
                v.runtime_index(receipts)

    def test_unsafe_archive_members(self):
        for name, kind in [('../escape', tarfile.REGTYPE), ('/absolute', tarfile.REGTYPE),
                           ('blobs/sha256/' + 'a' * 64, tarfile.SYMTYPE), ('index.json', tarfile.LNKTYPE),
                           ('script.sh', tarfile.REGTYPE), ('index.json', tarfile.FIFOTYPE)]:
            with self.subTest(name=name, kind=kind):
                archive = self.root / 'unsafe.tar'
                with tarfile.open(archive, 'w') as stream:
                    member = tarfile.TarInfo(name)
                    member.type = kind
                    member.linkname = '/escape'
                    stream.addfile(member)
                destination = self.root / ('unsafe-' + str(len(list(self.root.iterdir()))))
                with self.assertRaises(ValueError):
                    v.unpack(archive, destination)

    def test_duplicate_archive_member(self):
        with tarfile.open(self.root / 'unsafe.tar', 'w') as stream:
            for _ in range(2):
                member = tarfile.TarInfo('index.json')
                member.size = 2
                stream.addfile(member, io.BytesIO(b'{}'))
        with self.assertRaises(ValueError):
            v.unpack(self.root / 'unsafe.tar', self.root / 'unsafe')

    def test_scan_policy_includes_unfixed(self):
        report = {'SchemaVersion': 2, 'Results': [{'Vulnerabilities': [{
            'Severity': 'HIGH', 'VulnerabilityID': 'CVE-test', 'PkgName': 'lib-test', 'FixedVersion': ''}]}]}
        with self.assertRaises(ValueError):
            v.scan_policy(report, [], self.image['digest'])
        exception = {'manifest_digest': self.image['digest'], 'vulnerability': 'CVE-test', 'package': 'lib-test',
                     'owner': 'operator', 'reason': 'reviewed risk', 'expires': '2099-01-01'}
        v.scan_policy(report, [exception], self.image['digest'])
        for bad in [dict(exception, expires='2000-01-01'), dict(exception, reason=''), dict(exception, package='*'), dict(exception, typo='value')]:
            with self.assertRaises(ValueError):
                v.scan_policy(report, [bad], self.image['digest'])
        with self.assertRaises(ValueError):
            v.scan_policy(report, [dict(exception, manifest_digest='sha256:' + 'd' * 64)], self.image['digest'])
        with self.assertRaises(ValueError):
            v.scan_policy({}, [], self.image['digest'])

    def test_smoke_borrowed_image_not_built_or_deleted(self):
        app = self.root / 'app'
        app.mkdir()
        for name in ('Dockerfile', '.dockerignore', 'Cargo.lock', '.tool-versions'):
            (app / name).touch()
        (app / 'edgezero.toml').write_text('[app]\nname="test-app"\n')
        (app / 'Cargo.toml').write_text('[workspace.dependencies]\n')
        binaries = self.root / 'bin'
        binaries.mkdir()
        log = self.root / 'docker-calls'
        docker = binaries / 'docker'
        docker.write_text('''#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$CALLS"
case "$*" in
  'info --format '*) echo x86_64 ;;
  'image inspect supplied-image --format '*) echo linux/arm64 ;;
esac
''')
        docker.chmod(0o755)
        result = subprocess.run(['bash', str(ROOT / 'scripts/test_generated_axum_container.sh'), str(app),
                                 '--image', 'supplied-image', '--platform', 'linux/amd64'],
                                env=dict(os.environ, PATH=str(binaries) + ':' + os.environ['PATH'], CALLS=str(log)),
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Image/platform mismatch', result.stderr)
        calls = log.read_text()
        self.assertNotIn('image rm supplied-image', calls)
        self.assertNotIn('-t supplied-image', calls)


if __name__ == '__main__':
    unittest.main()
