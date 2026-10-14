"""Transport/cache contract; actual statistics are tested in the shared Rust crate."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec=importlib.util.spec_from_file_location('remote_usage',Path(__file__).resolve().parents[1]/'src-tauri/src/modules/remote_usage.py')
usage=importlib.util.module_from_spec(spec)
spec.loader.exec_module(usage)

class UsageTransportTest(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory()
        self.home=Path(self.temp.name).resolve()
        self.root=self.home/'codex'; self.root.mkdir()
        self.version='a'*64
        identity=hashlib.sha256(str(self.root).encode()).hexdigest()[:24]
        self.cache=self.home/'.cache/cockpit-tools/session-usage'/identity
        self.request={'codex_home':str(self.root),'version':self.version,'action':'sync',
                      'instanceId':'fixture','instanceName':'Fixture','query':{'fromTimestamp':10}}
        self.home_patch=patch.object(Path,'home',return_value=self.home);self.home_patch.start()
    def tearDown(self):
        self.home_patch.stop();self.temp.cleanup()
    def test_missing_binary_requests_sources_without_scanning_rollouts(self):
        self.assertEqual(usage.main(self.request),{'needsInstall':True})
        self.assertEqual(list(self.root.iterdir()),[])
    def test_cached_helper_receives_paths_and_query_not_transcripts(self):
        self.cache.mkdir(parents=True)
        binary=self.cache/('helper-'+self.version)
        binary.write_text('#!/usr/bin/env python3\nimport json,sys\np=json.load(sys.stdin)\nprint(json.dumps(p))\n')
        binary.chmod(0o700)
        with patch.object(usage,'build_helper',side_effect=AssertionError('must not rebuild')):
            result=usage.main(self.request)['report']
        self.assertEqual(result['codexHome'],str(self.root))
        self.assertEqual(result['query'],{'fromTimestamp':10})
        self.assertEqual(result['dbPath'],str(self.cache/'usage.sqlite'))
        self.assertNotIn('files',result)
        self.assertNotIn('sources',result)
    def test_bad_source_bundle_cannot_install_or_replace_binary(self):
        sources={name:'' for name in usage.SOURCE_NAMES}
        with self.assertRaisesRegex(ValueError,'校验失败'):
            usage.main(dict(self.request,sources=sources))
        self.assertFalse((self.cache/('helper-'+self.version)).exists())
    def test_digest_covers_filenames_and_contents(self):
        sources={name:'fixture' for name in usage.SOURCE_NAMES}
        first=usage.source_digest(sources)
        sources['src/lib.rs']+='changed'
        self.assertNotEqual(first,usage.source_digest(sources))
    def test_rollout_reader_is_hashed_and_transported_in_complete_source_bundle(self):
        sources={
            'Cargo.toml':'fixture manifest', 'Cargo.lock':'fixture lock',
            'src/lib.rs':'pub mod rollout;', 'src/main.rs':'fixture main',
            'src/rollout.rs':'fixture reader',
        }
        version=usage.source_digest(sources)
        changed=dict(sources, **{'src/rollout.rs':'changed reader'})
        self.assertNotEqual(version,usage.source_digest(changed))
        self.cache.mkdir(parents=True)
        binary=self.cache/('helper-'+version)
        def compile_fixture(command, **options):
            root=options['cwd']
            self.assertEqual(command,['/fixture/cargo','build','--release','--locked',
                                      '--manifest-path',str(root/'Cargo.toml')])
            self.assertEqual({name:(root/name).read_text() for name in sources},sources)
            built=root/'target/release/cockpit-session-usage'
            built.parent.mkdir(parents=True)
            built.write_text('fixture helper')
            return Mock(**{'wait.return_value':0})
        with patch.object(usage.shutil,'which',return_value='/fixture/cargo'), \
             patch.object(usage.subprocess,'Popen',side_effect=compile_fixture) as compile_call:
            usage.build_helper(self.cache,binary,sources,version)
            compile_call.assert_called_once()
        self.assertEqual(binary.read_text(),'fixture helper')
        incomplete=dict(sources)
        incomplete.pop('src/rollout.rs')
        with patch.object(usage.subprocess,'Popen') as compile_call:
            with self.assertRaisesRegex(ValueError,'校验失败'):
                usage.build_helper(self.cache,binary,incomplete,version)
            compile_call.assert_not_called()
        self.assertEqual(binary.read_text(),'fixture helper')

if __name__=='__main__': unittest.main()
