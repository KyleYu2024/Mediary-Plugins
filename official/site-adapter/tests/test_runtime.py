#!/usr/bin/env python3
"""Exercise the adapter binary against isolated local HTTP fixtures, without live accounts."""
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('SITE_ADAPTER_BINARY', ROOT / 'target/debug/site-adapter-plugin'))
TORRENT = b'd4:infod6:lengthi1e4:name5:a.mkv12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee'


class Fixture(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.requests.append((self.path, self.headers.get('Cookie'), self.headers.get('Authorization')))
        if self.path == '/redirect':
            self.send_response(302)
            self.send_header('Location', 'http://unlisted.invalid/private')
            self.end_headers()
            return
        if self.path == '/download/42':
            self.send_response(200)
            self.send_header('Content-Type', 'application/x-bittorrent')
            self.end_headers()
            self.wfile.write(TORRENT)
            return
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps({'data': [{'id': 42, 'name': 'Fixture Show S01E02 1080p',
                                               'size': 123456789, 'seeders': 7, 'leechers': 2}]}).encode())


def package(identifier='fixture', version='1.0.0'):
    return {
        'format_version': 1, 'id': identifier, 'name': 'Fixture', 'version': version,
        'domains': ['127.0.0.1'], 'network_domains': ['localhost'],
        'operations': ['latest', 'search', 'download'],
        'settings_fields': [{'key': 'private_value', 'label': 'Private', 'secret': True}],
        'script': '''
fn latest(ctx) {
    let response = http_get("/catalog");
    if response.status != 200 { throw "unexpected status"; }
    let items = [];
    for row in response.json.data {
        items.push(#{id:row.id.to_string(), title:row.name, size:row.size,
          page_url:"/torrents/" + row.id.to_string(), seeders:row.seeders,
          leechers:row.leechers, downloadvolumefactor:0.0});
    }
    #{items:items}
}
fn search(ctx) {
    let items = [];
    for item in latest(ctx).items {
        if item.title.to_lower().contains(ctx.input.keyword.to_lower()) { items.push(item); }
    }
    #{items:items}
}
fn download(ctx) { #{torrent_base64:torrent_get("/download/" + ctx.input.torrent_id)} }
''',
    }


class AdapterTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.http = ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        cls.thread = threading.Thread(target=cls.http.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.http.shutdown()
        cls.http.server_close()

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='mediary-adapter-test-')
        self.addCleanup(self.directory.cleanup)
        self.env = {**os.environ, 'MEDIARY_PLUGIN_DATA_DIR': self.directory.name}
        self.site = {'id': 1, 'name': 'Fixture', 'url': f'http://127.0.0.1:{self.http.server_port}',
                     'cookie': 'session=LOCAL_TEST_COOKIE', 'authorization': 'Bearer LOCAL_TEST_AUTH',
                     'api_key': 'LOCAL_TEST_KEY', 'rss_url': 'LOCAL_TEST_RSS', 'use_proxy': False}
        Fixture.requests.clear()

    def invoke(self, action, payload, fail=False):
        result = subprocess.run([str(BINARY)], input=json.dumps(payload), text=True,
                                capture_output=True, timeout=15,
                                env={**self.env, 'MEDIARY_PLUGIN_ACTION': action})
        if fail:
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('LOCAL_TEST_', result.stderr)
            return result.stderr
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def install(self, adapter=None):
        adapter = adapter or package()
        self.invoke('import', {'package': adapter})
        self.invoke('toggle', {'id': adapter['id'], 'enabled': True})

    def dispatch(self, operation='latest', args=None, fail=False):
        return self.invoke('site-dispatch', {'contract_version': 1, 'site': self.site,
                            'operation': operation, 'input': args or {}}, fail)

    def test_empty_and_disabled_fall_back_but_enabled_adapter_uses_current_credentials(self):
        self.assertFalse(self.dispatch()['handled'])
        self.invoke('import', {'package': package()})
        self.assertFalse(self.dispatch()['handled'])
        self.invoke('toggle', {'id': 'fixture', 'enabled': True})
        latest = self.dispatch()
        self.assertEqual(latest['items'][0]['id'], '42')
        self.assertEqual(latest['items'][0]['seeders'], 7)
        self.assertEqual(Fixture.requests[-1][1:], ('session=LOCAL_TEST_COOKIE', 'Bearer LOCAL_TEST_AUTH'))
        self.site['cookie'] = 'session=LOCAL_TEST_REFRESHED'
        self.assertEqual(len(self.dispatch('search', {'keyword': 'S01E02'})['items']), 1)
        self.assertEqual(Fixture.requests[-1][1], 'session=LOCAL_TEST_REFRESHED')
        self.assertEqual(self.dispatch('search', {'keyword': 'absent'})['items'], [])
        self.assertFalse(self.dispatch('profile')['handled'])
        torrent = self.dispatch('download', {'adapter_id': 'fixture', 'torrent_id': '42'})
        self.assertEqual(base64.b64decode(torrent['torrent_base64']), TORRENT)
        self.assertFalse(self.dispatch('download', {'adapter_id': 'different', 'torrent_id': '42'})['handled'])

    def test_export_excludes_local_secrets_and_round_trips_to_another_installation(self):
        self.install()
        self.invoke('settings', {'id': 'fixture', 'settings': {'private_value': 'LOCAL_TEST_PRIVATE'}})
        self.dispatch()
        exported = self.invoke('export', {'id': 'fixture'})
        self.assertNotIn('LOCAL_TEST_', json.dumps(exported))
        self.assertEqual(exported['package']['script'], package()['script'])
        with tempfile.TemporaryDirectory() as other:
            self.env['MEDIARY_PLUGIN_DATA_DIR'] = other
            self.invoke('import', exported)
            self.assertFalse(self.dispatch()['handled'])
            self.invoke('toggle', {'id': 'fixture', 'enabled': True})
            self.assertTrue(self.dispatch()['handled'])

    def test_conflict_replace_and_rollback_preserve_local_settings(self):
        self.install()
        self.invoke('settings', {'id': 'fixture', 'settings': {'private_value': 'LOCAL_TEST_PRIVATE'}})
        self.invoke('import', {'package': package('other')})
        self.invoke('toggle', {'id': 'other', 'enabled': True}, fail=True)
        replacement = package(version='1.0.1')
        self.invoke('import', {'package': replacement}, fail=True)
        self.invoke('import', {'package': replacement, 'replace': True})
        self.assertFalse(self.dispatch()['handled'])
        data = json.loads((Path(self.directory.name) / 'adapters.json').read_text())
        self.assertEqual(data['fixture']['settings']['private_value'], 'LOCAL_TEST_PRIVATE')
        self.invoke('rollback', {'id': 'fixture'})
        self.assertEqual(self.invoke('export', {'id': 'fixture'})['package']['version'], '1.0.0')

    def test_cross_origin_requests_do_not_receive_site_credentials_and_redirects_are_checked(self):
        adapter = package()
        other = f'http://localhost:{self.http.server_port}/catalog'
        adapter['script'] = adapter['script'].replace('http_get("/catalog")', f'http_get("{other}")')
        self.install(adapter)
        self.dispatch()
        self.assertEqual(Fixture.requests[-1][1:], (None, None))
        adapter['script'] = package()['script'].replace('http_get("/catalog")', 'http_get("/redirect")')
        self.invoke('import', {'package': adapter, 'replace': True})
        self.invoke('toggle', {'id': 'fixture', 'enabled': True})
        self.dispatch(fail=True)
        self.assertEqual(Fixture.requests[-1][0], '/redirect')

    def test_operation_limits_and_no_file_imports_and_preview_download_rejection(self):
        adapter = package()
        adapter['script'] = 'import "private" as module; ' + adapter['script']
        self.invoke('import', {'package': adapter}, fail=True)
        adapter = package()
        adapter['script'] = adapter['script'].replace('let response = http_get("/catalog");', 'while true {} let response = http_get("/catalog");')
        self.install(adapter)
        self.dispatch(fail=True)
        self.invoke('import', {'package': package(), 'replace': True})
        self.invoke('toggle', {'id': 'fixture', 'enabled': True})
        self.site['preview'] = True
        self.dispatch('download', {'adapter_id': 'fixture', 'torrent_id': '42'}, fail=True)
        self.assertFalse(any(path.startswith('/download') for path, *_ in Fixture.requests))


if __name__ == '__main__':
    if not BINARY.is_file():
        raise SystemExit('请先编译插件：cargo build --manifest-path plugins/site-adapter/Cargo.toml --locked')
    unittest.main()
