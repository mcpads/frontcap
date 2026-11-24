"""Regression: zero-mtime source snapshots must not reuse a different workspace binary."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from native_cargo_build import build


class FrozenSourceBuildTest(unittest.TestCase):
    def test_changed_snapshot_produces_changed_binary_in_shared_cache(self):
        with tempfile.TemporaryDirectory(prefix='frontcap-cargo-source-') as directory:
            root = Path(directory)
            cache = root/'cache'
            for index, (text, enabled) in enumerate([('before', False), ('after', True), ('after', False)]):
                source = root/str(index)
                (source/'src').mkdir(parents=True)
                (source/'Cargo.toml').write_text(
                    '[package]\nname="snapshot-consumer"\nversion="0.0.0"\nedition="2024"\n[features]\nfault-injection=[]\n')
                (source/'src/main.rs').write_text('fn main() { println!("'+text+' {}", cfg!(feature = "fault-injection")); }\n')
                subprocess.run(['cargo', 'generate-lockfile', '--offline'], cwd=source, check=True)
                files = {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                         for p in source.rglob('*') if p.is_file()}
                for name in files:
                    os.utime(source/name, (0, 0))
                request = root/(str(index)+'.json')
                request.write_text(json.dumps(dict(source=dict(
                    files=files, archive_sha256=hashlib.sha256(json.dumps(files).encode()).hexdigest()))))
                (root/('report-'+str(index))).mkdir()
                report = root/('report-'+str(index))/'build.json'
                features = ['fault-injection'] if enabled else []
                expected = text+' '+str(enabled).lower()
                build(source, request, cache, ['snapshot-consumer'], 1, report, features=features)
                self.assertEqual(subprocess.check_output([cache/'debug/snapshot-consumer'], text=True).strip(), expected)
                self.assertTrue(json.loads(report.read_text())['workspace_cleaned'])
                self.assertEqual(json.loads(report.read_text())['features'], features)
                self.assertEqual(subprocess.check_output([report.parent/'binaries/snapshot-consumer'], text=True).strip(), expected)
                # Verification is by content, not mtime or a successful cache hit.
                (source/'src/main.rs').write_text('fn main() {}\n')
                with self.assertRaises(ValueError):
                    build(source, request, cache, ['snapshot-consumer'], 1, root/'invalid.json')


if __name__ == '__main__':
    unittest.main()
