"""Run without AstrBot: exercise actual plugin methods extracted from its AST."""
import ast
import asyncio
import copy
import logging
from pathlib import Path
import unittest
import urllib.error

SOURCE = Path(__file__).resolve().parents[1] / 'astrbot_plugin_sign-system' / 'main.py'
tree = ast.parse(SOURCE.read_text(encoding='utf-8'))
plugin_class = next(node for node in tree.body if isinstance(node, ast.ClassDef))
plugin_class.decorator_list = []
plugin_class.bases = []
for node in plugin_class.body:
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        node.decorator_list = [d for d in node.decorator_list if isinstance(d, ast.Name) and d.id == 'staticmethod']
        node.returns = None
        for arg in node.args.args:
            arg.annotation = None
namespace = {'asyncio': asyncio, 'json': __import__('json'), 'logger': logging.getLogger('test'), 'urllib': __import__('urllib'), 'os': __import__('os')}
exec(compile(ast.fix_missing_locations(ast.Module(body=[plugin_class], type_ignores=[])), str(SOURCE), 'exec'), namespace)
Plugin = namespace['SigninMonitor']


def account(aid='a', status='absent'):
    return {'account_id': aid, 'name': aid, 'status': status}


def call(accounts=None, expired=False):
    return {'id': 'r', 'course_title': '数学', 'title': '课堂签到', 'is_expired': expired, 'accounts': accounts or [account()]}


class PartialSnapshotTests(unittest.IsolatedAsyncioTestCase):
    def make_plugin(self):
        p = Plugin.__new__(Plugin)
        p.config = {'interval': 10}
        p._seen = {}
        p._has_snapshot = False
        p._pending_notices = []
        p._account_health = {}
        return p

    async def run_snapshots(self, p, snapshots):
        snapshot_list = copy.deepcopy(snapshots)
        snapshots = iter(snapshot_list)
        messages = []
        def fetch():
            item = next(snapshots)
            if isinstance(item, Exception):
                raise item
            return item
        async def notify(text):
            messages.append(text)
            return True
        p._fetch_rollcalls = fetch
        p._notify = notify
        original_sleep = asyncio.sleep
        sleep_count = 0
        async def end_after_snapshots(_):
            nonlocal sleep_count
            sleep_count += 1
            if sleep_count >= len(snapshot_list):
                raise asyncio.CancelledError()
        asyncio.sleep = end_after_snapshots
        try:
            with self.assertRaises(asyncio.CancelledError):
                await p._poll_loop()
        finally:
            asyncio.sleep = original_sleep
        return messages

    async def test_monitor_response_parses_partial_health(self):
        p = self.make_plugin()
        p.config = {'base_url': 'http://example.test', 'api_token': 'test-token'}
        payload = {
            'rollcalls': [call()],
            'complete': False,
            'account_health': [{'account_id': 'a', 'name': '小明', 'state': 'expired'}],
        }
        class Response:
            headers = {'X-Rollcalls-Complete': 'false'}
            def __enter__(self):
                return self
            def __exit__(self, *_args):
                return False
            def read(self):
                import json
                return json.dumps(payload).encode('utf-8')
        seen = {}
        def fake_urlopen(request, timeout):
            seen['url'] = request.full_url
            seen['authorization'] = request.get_header('Authorization')
            seen['timeout'] = timeout
            return Response()
        import urllib.request
        original_urlopen = urllib.request.urlopen
        urllib.request.urlopen = fake_urlopen
        try:
            rollcalls, complete, health = p._fetch_rollcalls()
        finally:
            urllib.request.urlopen = original_urlopen
        self.assertEqual(seen['url'], 'http://example.test/api/rollcalls?monitor=1')
        self.assertEqual(seen['authorization'], 'Bearer test-token')
        self.assertEqual(seen['timeout'], 15)
        self.assertEqual(rollcalls[0]['id'], 'r')
        self.assertIs(complete, False)
        self.assertEqual(health[0]['state'], 'expired')

    async def test_partial_empty_preserves_call_and_explicit_end_works(self):
        p = self.make_plugin()
        messages = await self.run_snapshots(p, [([call()], True, []), ([], False, []), ([call(expired=True)], False, [])])
        self.assertEqual(sum('老师开始签到' in m for m in messages), 1)
        self.assertEqual(sum('签到已结束' in m for m in messages), 1)
        self.assertFalse(any('快照' in m or 'winSignIn API' in m for m in messages))
        self.assertTrue(p._seen['r']['is_expired'])

    async def test_missing_account_baseline_is_preserved(self):
        p = self.make_plugin()
        messages = await self.run_snapshots(p, [([call([account('a'), account('b')])], True, []), ([call([account('a')])], False, []), ([call([account('a'), account('b', 'on_call_fine')])], False, [])])
        self.assertTrue(any('b：未到 → 已签到' in m for m in messages))
        self.assertFalse(any('签到已结束' in m for m in messages))

    async def test_expiry_and_recovery_deduplicated_with_no_rollcalls(self):
        p = self.make_plugin()
        bad = [{'account_id': 'a', 'name': '小明', 'state': 'expired'}]
        good = [{'account_id': 'a', 'name': '小明', 'state': 'ok'}]
        messages = await self.run_snapshots(p, [([], False, bad), ([], False, bad), ([], True, good)])
        self.assertEqual(sum('账号登录已过期：小明' in m for m in messages), 1)
        self.assertFalse(any('账号状态已恢复' in m for m in messages))
        self.assertEqual(p._account_health['a']['state'], 'ok')

    async def test_api_outage_and_recovery_are_not_pushed(self):
        p = self.make_plugin()
        failure = urllib.error.URLError('offline')
        messages = await self.run_snapshots(p, [failure, failure, ([call()], False, [])])
        self.assertFalse(any('winSignIn API' in m or '无法连接' in m for m in messages))
        self.assertFalse(any('恢复' in m for m in messages))
        self.assertTrue(any('老师开始签到' in m for m in messages))

    async def test_snapshot_and_account_query_status_are_not_pushed(self):
        p = self.make_plugin()
        failed = [{'account_id': 'a', 'name': '小明', 'state': 'failed'}]
        healthy = [{'account_id': 'a', 'name': '小明', 'state': 'ok'}]
        messages = await self.run_snapshots(p, [([], False, failed), ([], True, healthy)])
        self.assertEqual(messages, [])

    async def test_failed_send_does_not_resend_successful_notice(self):
        p = self.make_plugin()
        attempts = []
        async def notify(text):
            attempts.append(text)
            if text == 'bad' and attempts.count('bad') == 1:
                raise OSError('send failed')
            return True
        p._notify = notify
        await p._deliver_notices(['good', 'bad'])
        self.assertEqual(p._pending_notices, ['bad'])
        await p._deliver_notices([])
        self.assertEqual(attempts, ['good', 'bad', 'bad'])
        self.assertEqual(p._pending_notices, [])


if __name__ == '__main__':
    unittest.main()
