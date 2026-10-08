#!/usr/bin/env python3
"""Verify this recipe's single-platform OCI bundles. No third-party dependencies.

Receipts/checksums are integrity checks, not authentication. The publisher must
select immutable artifacts from its approved same-run producer before calling us.
"""

import argparse
import datetime
import hashlib
import io
import json
import pathlib
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request

MANIFEST = 'application/vnd.oci.image.manifest.v1+json'
CONFIG = 'application/vnd.oci.image.config.v1+json'
INDEX = 'application/vnd.oci.image.index.v1+json'
LAYERS = {'application/vnd.oci.image.layer.v1.tar', 'application/vnd.oci.image.layer.v1.tar+gzip',
          'application/vnd.oci.image.layer.v1.tar+zstd'}
DIGEST = re.compile(r'sha256:[0-9a-f]{64}\Z')
PLATFORMS = {'linux/amd64', 'linux/arm64'}
SIDECARS = ('inputs.json', 'scan.json', 'scanner.json', 'sbom.json', 'provenance.json')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def checksum(path):
    require(path.is_file() and not path.is_symlink(), 'Expected regular artifact file')
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def load(path):
    require(path.is_file() and not path.is_symlink(), 'Expected regular JSON file')
    require(path.stat().st_size <= 16 * 1024 * 1024, f'JSON too large: {path.name}')
    return json.loads(path.read_text())


def unpack(archive, destination):
    # New output only. Reject links, special files, duplicate names and arbitrary payloads.
    destination.mkdir()
    seen = set()
    total = 0
    with tarfile.open(archive, 'r:*') as tar:
        for member in tar:
            name = member.name.removeprefix('./')
            if name in ('', '.') and member.isdir():
                continue
            path = pathlib.PurePosixPath(name)
            require(not path.is_absolute() and '..' not in path.parts and str(path) == name.rstrip('/'), 'Unsafe archive path')
            require(name.rstrip('/') not in seen, 'Duplicate archive member')
            seen.add(name.rstrip('/'))
            require(len(seen) <= 10000, 'Too many archive members')
            valid_dir = name.rstrip('/') in ('blobs', 'blobs/sha256')
            valid_file = name in ('oci-layout', 'index.json') or re.fullmatch(r'blobs/sha256/[0-9a-f]{64}', name)
            require((member.isdir() and valid_dir) or (member.isreg() and valid_file and not member.sparse), 'Unexpected archive member')
            total += member.size
            require(total <= 4 * 1024 ** 3, 'Archive exceeds 4 GiB limit')
            target = destination.joinpath(*path.parts)
            if member.isdir():
                target.mkdir(exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with tar.extractfile(member) as source, target.open('xb') as output:
                    for chunk in iter(lambda: source.read(1024 * 1024), b''):
                        output.write(chunk)
                require(target.stat().st_size == member.size, 'Truncated archive member')


def blob(layout, descriptor, media_types):
    digest = descriptor['digest']
    require(isinstance(digest, str) and DIGEST.fullmatch(digest), 'Unsupported blob digest')
    require(descriptor['mediaType'] in media_types, 'Unexpected blob media type')
    size = descriptor['size']
    require(type(size) is int and size >= 0, 'Invalid descriptor size')
    path = layout / 'blobs' / 'sha256' / digest[7:]
    require(path.is_file() and not path.is_symlink(), 'Missing regular blob')
    require(path.stat().st_size == size and checksum(path) == digest[7:], 'Blob digest/size mismatch')
    return path


def describe(layout, platform):
    require(platform in PLATFORMS, 'Unsupported platform')
    require(load(layout / 'oci-layout') == {'imageLayoutVersion': '1.0.0'}, 'Unsupported OCI layout')
    index = load(layout / 'index.json')
    require(index['schemaVersion'] == 2 and index.get('mediaType', INDEX) == INDEX, 'Invalid OCI index')
    require(len(index['manifests']) == 1, 'Expected exactly one runtime manifest, no attestations')
    descriptor = index['manifests'][0]
    manifest = load(blob(layout, descriptor, {MANIFEST}))
    require(manifest['schemaVersion'] == 2 and manifest['mediaType'] == MANIFEST, 'Invalid runtime manifest')
    config = load(blob(layout, manifest['config'], {CONFIG}))
    actual = f'{config["os"]}/{config["architecture"]}'
    require(actual == platform, 'Config/platform mismatch')
    if 'platform' in descriptor:
        require(f'{descriptor["platform"]["os"]}/{descriptor["platform"]["architecture"]}' == platform, 'Descriptor/platform mismatch')
    require(manifest['layers'], 'Runtime image has no layers')
    for layer in manifest['layers']:
        blob(layout, layer, LAYERS)
    return {'platform': platform, 'manifest_digest': descriptor['digest'],
            'manifest_size': descriptor['size'], 'config_digest': manifest['config']['digest'],
            'revision_label': config.get('config', {}).get('Labels', {}).get('org.opencontainers.image.revision')}


def verify_pulled(layout, inspection, platform, reference):
    info = describe(layout, platform)
    image = inspection[0]
    require(reference.endswith('@' + info['manifest_digest']), 'Expected pulled child digest mismatch')
    require(reference in image['RepoDigests'], 'Pulled digest reference mismatch')
    require(f'{image["Os"]}/{image["Architecture"]}' == platform, 'Pulled platform mismatch')
    config = load(layout / 'blobs/sha256' / info['config_digest'][7:])
    require(image['RootFS']['Layers'] == config['rootfs']['diff_ids'], 'Pulled filesystem identity mismatch')
    for key in ('User', 'Env', 'Entrypoint', 'Cmd', 'Labels', 'WorkingDir', 'ExposedPorts', 'StopSignal'):
        require(image['Config'].get(key) == config['config'].get(key), 'Pulled runtime config mismatch')
    # Docker's classic store uses config digest for Id; its containerd store uses
    # the manifest digest. Neither representation is an identity conversion.
    if 'Descriptor' in image:
        require(image['Descriptor']['digest'] == info['manifest_digest'], 'Pulled manifest mismatch')
    return info


def scan_policy(report, exceptions, manifest_digest, today=None, *, policy='strict'):
    require(policy in ('strict', 'report-only'), 'Invalid scan policy')
    today = today or datetime.datetime.now(datetime.timezone.utc).date()
    require(report.get('SchemaVersion') == 2 and isinstance(report.get('Results'), list) and report['Results'], 'Missing Trivy results')
    require(isinstance(exceptions, list), 'Exceptions must be a list')
    allowed = set()
    for entry in exceptions:
        require(set(entry) == {'manifest_digest', 'vulnerability', 'package', 'owner', 'reason', 'expires'}, 'Invalid exception fields')
        require(all(isinstance(v, str) and v.strip() for v in entry.values()), 'Empty exception field')
        require(DIGEST.fullmatch(entry['manifest_digest']), 'Invalid exception artifact scope')
        require(not any(c in entry['package'] + entry['vulnerability'] for c in '*?[]'), 'Wildcard scan exception')
        require(datetime.date.fromisoformat(entry['expires']) >= today, 'Expired scan exception')
        key = (entry['manifest_digest'], entry['vulnerability'], entry['package'])
        require(key not in allowed, 'Duplicate scan exception')
        allowed.add(key)
    counts = {'HIGH': 0, 'CRITICAL': 0}
    fixable = 0
    for result in report['Results']:
        for finding in result.get('Vulnerabilities') or []:
            require(finding['Severity'] in ('UNKNOWN', 'LOW', 'MEDIUM', 'HIGH', 'CRITICAL'), 'Invalid vulnerability severity')
            if finding['Severity'] in counts:
                key = (manifest_digest, finding['VulnerabilityID'], finding['PkgName'])
                counts[finding['Severity']] += 1
                fixable += bool(finding.get('FixedVersion'))
                if policy == 'strict':
                    require(key in allowed, f'Blocked {finding["Severity"]} vulnerability: {finding["VulnerabilityID"]} in {finding["PkgName"]}')
    if policy == 'report-only':
        print(f'WARNING: report-only scan: {counts["HIGH"]} HIGH, {counts["CRITICAL"]} CRITICAL package findings, '
              f'{fixable} with fixes. Packaging evidence only; not approved for release.', file=sys.stderr)


def seal(bundle, revision, run_id, platform):
    require(re.fullmatch(r'[0-9a-f]{40}', revision), 'Expected immutable source commit')
    require(re.fullmatch(r'[0-9]+(?:-[0-9]+)?|local', run_id), 'Invalid run identity')
    info = describe(bundle / 'oci', platform)
    require(info['revision_label'] == revision, 'Image/source revision mismatch')
    inputs = load(bundle / 'inputs.json')
    check_inputs(inputs, revision, run_id)
    provenance = load(bundle / 'provenance.json')
    require(provenance['subject'] == info['manifest_digest'] and provenance['revision'] == revision, 'Provenance subject/source mismatch')
    hashes = {name: checksum(bundle / name) for name in SIDECARS}
    receipt = {'schema': 1, 'revision': revision, 'run_id': run_id, 'checks': 'packaging-passed',
               **info, 'archive_sha256': checksum(bundle / 'image.tar'), 'sidecars': hashes}
    (bundle / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    return receipt


def check_inputs(inputs, revision, run_id):
    require(inputs['revision'] == revision, 'Input/source revision mismatch')
    require(type(inputs['dirty']) is bool, 'Input state missing')
    require(run_id == 'local' or not inputs['dirty'], 'Dirty source cannot be a trusted release input')
    for name in ('source_sha256', 'context_sha256', 'lock_sha256', 'dockerfile_sha256'):
        require(isinstance(inputs[name], str) and re.fullmatch(r'[0-9a-f]{64}', inputs[name]), 'Input hash missing')
    require(isinstance(inputs['toolchain'], str) and re.search(r'^rust [0-9]+\.[0-9]+\.[0-9]+$', inputs['toolchain'], re.M), 'Toolchain trace missing')
    bases = inputs['base_images']
    require(isinstance(bases, list) and bases and all(isinstance(base, str) and
            re.fullmatch(r'[^\s@]+@sha256:[0-9a-f]{64}', base) for base in bases), 'Pinned base trace missing')


def verify(bundle, revision, run_id, platform, exceptions, *, policy='strict'):
    receipt = load(bundle / 'receipt.json')
    require(receipt['schema'] == 1 and receipt['checks'] == 'packaging-passed', 'Acceptance missing')
    require(receipt['revision'] == revision and receipt['run_id'] == run_id, 'Unauthorized source/run')
    require(receipt['platform'] == platform, 'Receipt/platform mismatch')
    require(checksum(bundle / 'image.tar') == receipt['archive_sha256'], 'Archive checksum mismatch')
    # Never use artifact-provided unpacked paths. Recreate from the retained archive.
    with tempfile.TemporaryDirectory() as scratch:
        layout = pathlib.Path(scratch) / 'oci'
        unpack(bundle / 'image.tar', layout)
        info = describe(layout, platform)
        config = load(layout / 'blobs/sha256' / info['config_digest'][7:])
    require(all(receipt[k] == v for k, v in info.items()), 'Receipt/image mismatch')
    require(info['revision_label'] == revision, 'Image revision label mismatch')
    require(set(receipt['sidecars']) == set(SIDECARS), 'Missing sidecar identities')
    for name, expected in receipt['sidecars'].items():
        require(checksum(bundle / name) == expected, 'Metadata checksum mismatch')
    inputs = load(bundle / 'inputs.json')
    check_inputs(inputs, revision, run_id)
    provenance = load(bundle / 'provenance.json')
    require(provenance['subject'] == info['manifest_digest'] and provenance['revision'] == revision, 'Provenance subject/source mismatch')
    require(provenance['inputs'] == inputs and provenance['run_id'] == run_id, 'Provenance input/run mismatch')
    scanner = load(bundle / 'scanner.json')
    require(scanner.get('Version') and scanner.get('VulnerabilityDB', {}).get('UpdatedAt'), 'Scanner/database identity missing')
    require(scanner.get('SubjectManifestDigest') == info['manifest_digest'], 'Scanner subject mismatch')
    report = load(bundle / 'scan.json')
    require(report.get('Metadata', {}).get('ImageID') == info['config_digest'], 'Scan/image config mismatch')
    require(report.get('Metadata', {}).get('DiffIDs') == config['rootfs']['diff_ids'], 'Scan/filesystem mismatch')
    family = report.get('Metadata', {}).get('OS', {}).get('Family')
    require(family and any(r.get('Class') == 'os-pkgs' and r.get('Type') == family for r in report['Results']),
            'Supported OS scan missing')
    require(all(r.get('Class') in ('os-pkgs', 'lang-pkgs') and r.get('Target') for r in report['Results']),
            'Incomplete supported scan results')
    scan_policy(report, exceptions, info['manifest_digest'], policy=policy)
    sbom = load(bundle / 'sbom.json')
    require(sbom.get('bomFormat') == 'CycloneDX' and sbom.get('components'), 'SBOM missing')
    properties = sbom.get('metadata', {}).get('component', {}).get('properties', [])
    require(any(p.get('name') == 'aquasecurity:trivy:ImageID' and p.get('value') == info['config_digest']
                for p in properties), 'SBOM/image config mismatch')
    return receipt


def runtime_index(receipts):
    require(len(receipts) == 2 and {r['platform'] for r in receipts} == PLATFORMS, 'Both unique native variants required')
    require(len({(r['revision'], r['run_id']) for r in receipts}) == 1, 'Variant source/run mismatch')
    require(len({r['sidecars']['inputs.json'] for r in receipts}) == 1, 'Variant prepared input mismatch')
    return {'schemaVersion': 2, 'mediaType': INDEX, 'manifests': [
        {'mediaType': MANIFEST, 'digest': r['manifest_digest'], 'size': r['manifest_size'],
         'platform': {'os': 'linux', 'architecture': r['platform'].split('/')[1]}}
        for r in sorted(receipts, key=lambda r: r['platform'])]}


def release_record(source, bundles, metadata, revision, run_id, exceptions):
    # Recreate the preparation job's inputs from the trusted commit, never by
    # executing or unpacking application-provided code in the publisher.
    require(re.fullmatch(r'[0-9a-f]{40}', revision), 'Expected immutable source commit')
    raw = subprocess.run(['git', 'archive', '--format=tar', revision], cwd=source,
                         check=True, stdout=subprocess.PIPE).stdout
    files = {}
    with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
        for entry in archive:
            require(entry.isdir() or entry.isfile(), 'Release source must not contain links')
            if entry.isfile():
                files[entry.name] = archive.extractfile(entry).read()
    context = hashlib.sha256()
    for name, data in sorted(files.items()):
        context.update(name.encode() + b'\0' + data + b'\0')
    expected = {'source_sha256': hashlib.sha256(raw).hexdigest(), 'context_sha256': context.hexdigest(),
                'lock_sha256': hashlib.sha256(files['Cargo.lock']).hexdigest(),
                'dockerfile_sha256': hashlib.sha256(files['Dockerfile']).hexdigest(),
                'toolchain': files['.tool-versions'].decode(),
                'base_images': re.findall(r'^FROM (\S+)', files['Dockerfile'].decode(), re.M), 'dirty': False}
    run, attempt = run_id.split('-')
    artifact_ids(metadata, run, attempt, revision)
    receipts, children = [], []
    for bundle, platform in zip(bundles, ('linux/amd64', 'linux/arm64')):
        receipt = verify(bundle, revision, run_id, platform, exceptions)
        inputs = load(bundle / 'inputs.json')
        require(all(inputs[key] == value for key, value in expected.items()), 'Prepared context differs from trusted source')
        receipts.append(receipt)
        scanner = load(bundle / 'scanner.json')
        arch = platform.split('/')[1]
        artifact = next(a for a in metadata['artifacts'] if a['name'] == f'accepted-{arch}-{run}-{attempt}')
        children.append({'platform': platform, 'manifest_digest': receipt['manifest_digest'],
                         'config_digest': receipt['config_digest'], 'archive_sha256': receipt['archive_sha256'],
                         'sidecars': receipt['sidecars'],
                         'actions_artifact': {'id': artifact['id'], 'digest': artifact['digest'], 'name': artifact['name']},
                         'scanner': {'version': scanner['Version'], 'database': scanner['VulnerabilityDB']},
                         'tests': 'application-owned checks succeeded in native job'})
    serialized = (json.dumps(runtime_index(receipts), indent=2) + '\n').encode()
    return {'version': 1, 'scope': 'application container acceptance, not framework production certification',
            'source': {'revision': revision, **expected}, 'run_id': run_id, 'children': children,
            'index_digest': 'sha256:' + hashlib.sha256(serialized).hexdigest(),
            'scan_policy': 'High/Critical including unfixed; only independently trusted scoped exceptions',
            'exceptions': exceptions}


def check_tag(repository, tag, digest, auth_file=None):
    require(DIGEST.fullmatch(digest), 'Invalid expected index digest')
    require(re.fullmatch(r'[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,127}', tag), 'Invalid version tag')
    host, name = repository.split('/', 1)
    require(re.fullmatch(r'[a-z0-9]+(?:[._/-][a-z0-9]+)*', name), 'Invalid repository path')
    require(host == 'ghcr.io' or re.fullmatch(r'127\.0\.0\.1:[0-9]+', host), 'Unsupported registry')
    headers = {'Accept': ', '.join((INDEX, MANIFEST, 'application/vnd.docker.distribution.manifest.list.v2+json',
                                    'application/vnd.docker.distribution.manifest.v2+json'))}

    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            raise ValueError('Registry redirect is not permitted')

    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    if host == 'ghcr.io':
        require(auth_file is not None, 'GHCR auth file required')
        auth = load(auth_file)['auths']['ghcr.io']['auth']
        query = urllib.parse.urlencode({'service': host, 'scope': f'repository:{name}:pull'})
        request = urllib.request.Request(f'https://{host}/token?{query}', headers={'Authorization': 'Basic ' + auth})
        with opener.open(request, timeout=30) as response:
            token = json.loads(response.read(1_000_001))['token']
        require(isinstance(token, str) and token, 'Registry token missing')
        headers['Authorization'] = 'Bearer ' + token
    scheme = 'https' if host == 'ghcr.io' else 'http'
    request = urllib.request.Request(f'{scheme}://{host}/v2/{name}/manifests/{tag}', headers=headers)
    try:
        with opener.open(request, timeout=30) as response:
            raw = response.read(16_000_001)
        require(len(raw) <= 16_000_000, 'Registry manifest too large')
    except urllib.error.HTTPError as error:
        # Only a confirmed registry not-found response permits a first publish;
        # authentication, rate-limit, transport and service errors fail closed.
        require(error.code == 404, f'Registry tag check failed: HTTP {error.code}')
        errors = json.loads(error.read(1_000_001)).get('errors', [])
        require(errors and all(e.get('code') in ('MANIFEST_UNKNOWN', 'NAME_UNKNOWN') for e in errors),
                'Registry did not confirm tag absence')
        return {'state': 'absent'}
    require('sha256:' + hashlib.sha256(raw).hexdigest() == digest, 'Version tag identifies a different release')
    return {'state': 'same-index'}


def artifact_ids(metadata, run_id, attempt, revision=None):
    names = {f'accepted-{arch}-{run_id}-{attempt}' for arch in ('amd64', 'arm64')}
    selected = [a for a in metadata['artifacts'] if a['name'] in names]
    require(len(selected) == 2 and {a['name'] for a in selected} == names, 'Accepted artifacts missing or duplicated')
    for artifact in selected:
        require(type(artifact['id']) is int and artifact['id'] > 0 and not artifact['expired'], 'Invalid artifact identity')
        require(DIGEST.fullmatch(artifact['digest']), 'Actions artifact digest missing')
        require(str(artifact['workflow_run']['id']) == run_id, 'Artifact run mismatch')
        if revision is not None:
            require(artifact['workflow_run']['head_sha'] == revision, 'Artifact producer source mismatch')
    return ','.join(str(a['id']) for a in sorted(selected, key=lambda a: a['name']))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    extract = sub.add_parser('extract')
    extract.add_argument('archive', type=pathlib.Path)
    extract.add_argument('destination', type=pathlib.Path)
    extract.add_argument('--platform', required=True, choices=sorted(PLATFORMS))
    for command in ('seal', 'verify'):
        child = sub.add_parser(command)
        child.add_argument('bundle', type=pathlib.Path)
        child.add_argument('--revision', required=True)
        child.add_argument('--run-id', required=True)
        child.add_argument('--platform', required=True, choices=sorted(PLATFORMS))
        if command == 'verify':
            child.add_argument('--exceptions', type=pathlib.Path)
            child.add_argument('--scan-policy', choices=('strict', 'report-only'), default='strict')
    index = sub.add_parser('index')
    index.add_argument('bundles', nargs=2, type=pathlib.Path)
    index.add_argument('--revision', required=True)
    index.add_argument('--run-id', required=True)
    index.add_argument('--exceptions', type=pathlib.Path)
    index.add_argument('--scan-policy', choices=('strict', 'report-only'), default='strict')
    pulled = sub.add_parser('pulled')
    pulled.add_argument('layout', type=pathlib.Path)
    pulled.add_argument('inspection', type=pathlib.Path)
    pulled.add_argument('--platform', required=True, choices=sorted(PLATFORMS))
    pulled.add_argument('--reference', required=True)
    artifacts = sub.add_parser('artifact-ids')
    artifacts.add_argument('metadata', type=pathlib.Path)
    artifacts.add_argument('--run-id', required=True)
    artifacts.add_argument('--attempt', required=True)
    artifacts.add_argument('--revision')
    record = sub.add_parser('release-record')
    record.add_argument('source', type=pathlib.Path)
    record.add_argument('bundles', nargs=2, type=pathlib.Path)
    record.add_argument('metadata', type=pathlib.Path)
    record.add_argument('--revision', required=True)
    record.add_argument('--run-id', required=True)
    record.add_argument('--exceptions', type=pathlib.Path)
    tag = sub.add_parser('tag-check')
    tag.add_argument('repository')
    tag.add_argument('tag')
    tag.add_argument('digest')
    tag.add_argument('--auth-file', type=pathlib.Path)
    args = parser.parse_args()
    try:
        if args.command == 'release-record':
            exceptions = load(args.exceptions) if args.exceptions else []
            print(json.dumps(release_record(args.source, args.bundles, load(args.metadata),
                                           args.revision, args.run_id, exceptions), sort_keys=True))
            return
        if args.command == 'tag-check':
            print(json.dumps(check_tag(args.repository, args.tag, args.digest, args.auth_file)))
            return
        if args.command == 'artifact-ids':
            print(artifact_ids(load(args.metadata), args.run_id, args.attempt, args.revision))
            return
        if args.command == 'extract':
            unpack(args.archive, args.destination)
            result = describe(args.destination, args.platform)
        elif args.command == 'seal':
            result = seal(args.bundle, args.revision, args.run_id, args.platform)
        elif args.command == 'verify':
            exceptions = load(args.exceptions) if args.exceptions else []
            result = verify(args.bundle, args.revision, args.run_id, args.platform, exceptions, policy=args.scan_policy)
        elif args.command == 'pulled':
            result = verify_pulled(args.layout, load(args.inspection), args.platform, args.reference)
        else:
            exceptions = load(args.exceptions) if args.exceptions else []
            result = runtime_index([verify(bundle, args.revision, args.run_id, platform, exceptions, policy=args.scan_policy)
                                    for bundle, platform in zip(args.bundles, ('linux/amd64', 'linux/arm64'))])
        print(json.dumps(result, indent=2))
    except (ValueError, KeyError, TypeError, IndexError, OSError, tarfile.TarError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'Artifact verification failed: {error}\n')


if __name__ == '__main__':
    main()
