"""실제 앱/Tailscale을 호출하지 않는 runner 계약 fixture."""
import os
import json
import ast
import hashlib
import io
import tarfile
from unittest import mock
import signal
import subprocess
import time
from pathlib import Path
import tempfile
import types
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'relay-dev.sh'
SOURCE = SCRIPT.read_text().split("<<'PY_RELAY_RUNNER'\n", 1)[1].rsplit('\nPY_RELAY_RUNNER', 1)[0]
runner = types.ModuleType('relay_runner_fixture')
exec(compile(SOURCE, str(SCRIPT), 'exec'), runner.__dict__)

class RunnerContracts(unittest.TestCase):
    def test_env_rejects_injection_unknown_duplicate_and_oversize(self):
        route = 'a' * 32
        secret = 'b' * 64
        valid = f'DEPPY_RELAY_DEV_ROUTE={route}\nDEPPY_RELAY_DEV_ADMISSION={secret}\n'.encode()
        self.assertEqual(runner.parse_env(valid)['DEPPY_RELAY_DEV_ADMISSION'], secret)
        for invalid in [valid + b'EVIL=1\n', valid + valid, b'X=$(touch /tmp/injected)\n', valid.replace(b'a'*32,b'g'*32), b'x'*4097, valid.replace(b'b'*64,b'b'*63)]:
            with self.subTest(invalid_len=len(invalid)):
                with self.assertRaises(runner.RunnerError):
                    runner.parse_env(invalid)

    def test_original_env_comments_remain_data_without_shell_execution(self):
        data = '# 개발용 Relay 자격증명\nDEPPY_RELAY_DEV_ROUTE='.encode()+b'a'*32+b'\nDEPPY_RELAY_DEV_ADMISSION='+b'b'*64+b'\n'
        self.assertEqual(runner.parse_env(data)['DEPPY_RELAY_DEV_ROUTE'], 'a'*32)

    def test_private_file_rejects_loose_modes_symlink_and_large_content(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            file = root/'env'
            file.write_bytes(b'private')
            file.chmod(0o600)
            self.assertEqual(runner.read_private(file), b'private')
            link = root/'link'
            link.symlink_to(file)
            with self.assertRaises(runner.RunnerError): runner.read_private(link)
            file.chmod(0o644)
            with self.assertRaises(runner.RunnerError): runner.read_private(file)
            file.chmod(0o600)
            file.write_bytes(b'x'*4097)
            with self.assertRaises(runner.RunnerError): runner.read_private(file)

    def test_port_host_path_inputs_are_bounded(self):
        self.assertEqual(runner.port('8443'), 8443)
        self.assertEqual(runner.host('node.tail123.ts.net.'), 'node.tail123.ts.net')
        for value in ['0','65536','-1','1;touch x','1'*5000]:
            with self.assertRaises(runner.RunnerError): runner.port(value)
        for value in ['localhost','127.0.0.1','evil.ts.net/path','evil.ts.net\nX=1','x'*254+'.ts.net','a..ts.net']:
            with self.assertRaises(runner.RunnerError): runner.host(value)
        for value in ['/tmp/x\ny','/tmp/$(touch x)','/'+'x'*4096]:
            with self.assertRaises(runner.RunnerError): runner.safe_path(value)

    def test_process_identity_requires_nonce_role_start_and_script(self):
        script = '/tmp/fixture/scripts/relay-dev.sh'
        nonce = 'a'*32
        command = f'/usr/bin/python3 - {script} _worker relay {nonce}'
        identity = ['Tue Sep 8 12:00:00 2026', command]
        record = {'pid':123, 'nonce':nonce, 'role':'relay', 'identity':identity}
        self.assertTrue(runner.owned_process(record, identity, script))
        self.assertFalse(runner.owned_process(record, ['new time',command], script))
        forged = dict(record, nonce='b'*32)
        self.assertFalse(runner.owned_process(forged, identity, script))
        self.assertFalse(runner.owned_process(record, identity, '/other/relay-dev.sh'))

class LifecycleContracts(unittest.TestCase):
    def test_private_creation_is_exclusive_and_mode_0600(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)/'env'
            runner.write_private(path, b'one')
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            with self.assertRaises(runner.RunnerError): runner.write_private(path, b'two')
            self.assertEqual(path.read_bytes(), b'one')

    def test_worker_shutdown_preserves_an_unrelated_process(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root/'scripts').mkdir()
            script = root/'scripts/relay-dev.sh'
            script.write_text(SCRIPT.read_text())
            state = root/'.relay-dev'
            state.mkdir(mode=0o700)
            binary = state/'target/debug/relay-server'
            binary.parent.mkdir(parents=True)
            binary.write_text('#!/bin/sh\nexec sleep 60\n')
            binary.chmod(0o700)
            env = root/'.relay-dev.env'
            env.write_text('DEPPY_RELAY_DEV_ROUTE='+'a'*32+'\nDEPPY_RELAY_DEV_ADMISSION='+'b'*64+'\n')
            env.chmod(0o600)
            unrelated = subprocess.Popen(['sleep','60'])
            manager = runner.Runner(script)
            child = None
            try:
                child = manager.start_worker('relay')
                self.assertIsNotNone(child, '소유 supervisor가 실제로 시작되어야 한다')
                record = json.loads(runner.read_private(state/'relay.pid'))
                self.assertEqual(record['pid'], child.pid)
                before = time.monotonic()
                self.assertTrue(manager.stop_owned('relay'))
                child.wait(timeout=5)
                self.assertLess(time.monotonic()-before, 2, 'child는 SIGTERM을 상속 차단하지 않아야 한다')
                self.assertIsNone(unrelated.poll())
                self.assertFalse((state/'relay.pid').exists())
            finally:
                if child is not None and child.poll() is None:
                    child.terminate()
                    child.wait(timeout=5)
                unrelated.terminate()
                unrelated.wait(timeout=5)

    def test_forged_pid_file_does_not_signal_unrelated_process(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root/'scripts').mkdir()
            script = root/'scripts/relay-dev.sh'
            script.write_text(SCRIPT.read_text())
            manager = runner.Runner(script)
            unrelated = subprocess.Popen(['sleep','60'])
            try:
                path = manager.state/'relay.pid'
                path.write_text(json.dumps({'pid': unrelated.pid, 'nonce':'a'*32, 'role':'relay', 'identity':['wrong','wrong']}))
                path.chmod(0o600)
                with self.assertRaises(runner.RunnerError): manager.stop_owned('relay')
                self.assertIsNone(unrelated.poll())
                self.assertTrue(path.exists())
            finally:
                unrelated.terminate()
                unrelated.wait(timeout=5)

class OrchestrationContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root/'scripts').mkdir()
        self.script = self.root/'scripts/relay-dev.sh'
        self.script.write_text(SCRIPT.read_text())
        self.manager = runner.Runner(self.script)

    def fake_build(self, _argv, _env, _timeout):
        dist = self.manager.state/'dist'
        dist.mkdir(exist_ok=True)
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode='w:gz') as tar:
            for name in ['index.html','relay-shell.js','relay-terminal.js','relay-crypto.js','relay-shell.css','sw.js','manifest.webmanifest','mobile-theme.css']:
                body = b'fixture'
                item = tarfile.TarInfo('relay-shell/'+name)
                item.size = len(body)
                tar.addfile(item, io.BytesIO(body))
        content = raw.getvalue()
        digest = hashlib.sha256(content).hexdigest()
        name = 'relay-shell-'+digest+'.tar.gz'
        (dist/name).write_bytes(content)
        (dist/name).chmod(0o600)
        (dist/'relay-shell.manifest.json').write_text(json.dumps({'archive':name,'archive_sha256':digest}))
        (dist/'relay-shell.manifest.json').chmod(0o600)

    def test_repeated_shell_build_replaces_only_owned_assets(self):
        with mock.patch.object(runner, 'run_quiet', self.fake_build):
            self.manager.build_shell('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')
            self.manager.build_shell('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')
        self.assertEqual((self.manager.state/'serve/index.html').read_bytes(), b'fixture')

    def test_serve_changes_from_another_owner_are_never_removed(self):
        recorded = {'TCP':{'HTTPS':True}, 'Web':{}}
        path = self.manager.state/'serve.json'
        runner.write_private(path, json.dumps({'binary':'/fake/tailscale','ports':{'8443':recorded}}).encode())
        with mock.patch.object(self.manager,'tailscale',return_value='/fake/tailscale'), mock.patch.object(self.manager,'ts_json',return_value={'TCP':{'8443':{'HTTPS':False}}}), mock.patch.object(runner,'run_quiet') as command:
            with self.assertRaises(runner.RunnerError): self.manager.down_serves()
            command.assert_not_called()
        self.assertTrue(path.exists())

    def test_capture_output_and_time_are_bounded_without_echoing_child_secrets(self):
        with self.assertRaises(runner.RunnerError) as error:
            runner.capture(['python3','-c','print("private-marker"*10000)'], limit=32)
        self.assertNotIn('private-marker', str(error.exception))
        before = time.monotonic()
        with self.assertRaises(runner.RunnerError): runner.capture(['sleep','10'], timeout=0.05)
        self.assertLess(time.monotonic()-before, 4)

    def test_interrupted_build_reaps_only_its_process_group(self):
        child = mock.Mock(pid=12345)
        child.wait.return_value = 0
        with mock.patch.object(runner.subprocess, 'Popen', return_value=child), mock.patch.object(runner.os, 'waitid', side_effect=KeyboardInterrupt()), mock.patch.object(runner.os, 'killpg') as terminate:
            with self.assertRaises(KeyboardInterrupt): runner.run_quiet(['fixture'], {}, 5)
            self.assertEqual(terminate.call_args_list[0], mock.call(12345, signal.SIGTERM))

    def test_partial_serve_failure_cleans_owned_workers_and_first_port_only(self):
        config = {'TCP':{}, 'Web':{}}
        calls = []
        children = []
        def spawn(_role):
            child = subprocess.Popen(['sleep','60'])
            children.append(child)
            self.manager.workers.append(child)
            return child
        def command(argv, _env, _timeout):
            calls.append(argv)
            if '--bg' in argv:
                if '--https=10000' in argv: raise runner.RunnerError('fixture failure')
                config['TCP']['8443'] = {'HTTPS':True}
                config['Web']['node.tail.ts.net:8443'] = {'Handlers':{'/':{'Proxy':'http://127.0.0.1:9443'}}}
            elif 'off' in argv:
                config['TCP'].pop('8443')
                config['Web'].pop('node.tail.ts.net:8443')
        try:
            with mock.patch.object(self.manager,'origins',return_value=('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')), mock.patch.object(self.manager,'ts_json',side_effect=lambda *args:json.loads(json.dumps(config))), mock.patch.object(self.manager,'tailscale',return_value='/fake/tailscale'), mock.patch.object(self.manager,'build_shell'), mock.patch.object(self.manager,'start_worker',side_effect=spawn), mock.patch.object(self.manager,'wait_ready'), mock.patch.object(runner, 'run_quiet',side_effect=command), mock.patch.object(runner.socket, 'socket'):
                with self.assertRaises(runner.RunnerError): self.manager.up()
            self.assertEqual(len(children), 2)
            self.assertTrue(all(child.poll() is not None for child in children))
            off = [argv for argv in calls if 'off' in argv]
            self.assertEqual(off, [['/fake/tailscale','serve','--https=8443','off']])
            self.assertFalse((self.manager.state/'serve.json').exists())
        finally:
            for child in children:
                if child.poll() is None: child.terminate(); child.wait(timeout=5)

    def test_build_exit_failure_terminates_its_background_descendant(self):
        marker = self.root/'child.pid'
        command = f'sleep 60 & echo $! > "{marker}"; exit 7'
        try:
            with self.assertRaises(runner.RunnerError): runner.run_quiet(['/bin/sh','-c',command], os.environ, 5)
            pid = int(marker.read_text())
            observed = subprocess.run(['/bin/ps','-p',str(pid),'-o','stat='],capture_output=True,text=True).stdout.strip()
            self.assertTrue(not observed or observed.startswith('Z'), '실패한 build의 background child가 실행 중이면 안 된다')
        finally:
            if marker.exists():
                try: os.kill(int(marker.read_text()),signal.SIGKILL)
                except ProcessLookupError: pass

    def test_tampered_rebuild_preserves_previous_complete_assets(self):
        with mock.patch.object(runner,'run_quiet',self.fake_build):
            self.manager.build_shell('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')
        old = (self.manager.state/'serve/index.html').read_bytes()
        def malformed(*args):
            self.fake_build(*args)
            dist = self.manager.state/'dist'
            raw = io.BytesIO()
            with tarfile.open(fileobj=raw,mode='w:gz') as tar:
                item = tarfile.TarInfo('relay-shell/index.html'); item.size=3
                tar.addfile(item,io.BytesIO(b'new'))
            data=raw.getvalue(); digest=hashlib.sha256(data).hexdigest(); name='relay-shell-'+digest+'.tar.gz'
            (dist/name).write_bytes(data); (dist/name).chmod(0o600)
            (dist/'relay-shell.manifest.json').write_text(json.dumps({'archive':name,'archive_sha256':digest}))
            (dist/'relay-shell.manifest.json').chmod(0o600)
        with mock.patch.object(runner,'run_quiet',malformed):
            with self.assertRaises(runner.RunnerError): self.manager.build_shell('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')
        self.assertEqual((self.manager.state/'serve/index.html').read_bytes(),old)

    def test_changed_port_does_not_prevent_cleanup_of_another_owned_port(self):
        first={'TCP':{'HTTPS':True},'Web':{}}
        second={'TCP':{'HTTPS':True},'Web':{}}
        runner.write_private(self.manager.state/'serve.json',json.dumps({'binary':'/fake/tailscale','ports':{'8443':first,'10000':second}}).encode())
        config={'TCP':{'8443':{'HTTPS':False},'10000':{'HTTPS':True}},'Web':{}}
        with mock.patch.object(self.manager,'tailscale',return_value='/fake/tailscale'), mock.patch.object(self.manager,'ts_json',return_value=config), mock.patch.object(runner,'run_quiet') as command:
            with self.assertRaises(runner.RunnerError): self.manager.down_serves()
            command.assert_called_once_with(['/fake/tailscale','serve','--https=10000','off'],os.environ,15)
        self.assertEqual(set(json.loads(runner.read_private(self.manager.state/'serve.json'))['ports']),{'8443'})

    def test_port_claimed_during_build_is_rechecked_before_serve(self):
        config={'TCP':{},'Web':{}}
        def build(*_): config['TCP']['8443']={'HTTPS':False}
        with mock.patch.object(self.manager,'origins',return_value=('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')), mock.patch.object(self.manager,'ts_json',side_effect=lambda *args:json.loads(json.dumps(config))), mock.patch.object(self.manager,'tailscale',return_value='/fake/tailscale'), mock.patch.object(self.manager,'build_shell',side_effect=build), mock.patch.object(self.manager,'start_worker'), mock.patch.object(self.manager,'wait_ready'), mock.patch.object(runner.socket,'socket'), mock.patch.object(runner,'run_quiet') as command:
            with self.assertRaises(runner.RunnerError): self.manager.up()
            command.assert_not_called()

    def test_serve_success_then_status_failure_leaves_pending_recovery_record(self):
        applied=False
        def command(*_):
            nonlocal applied
            applied=True
        def status(*_):
            if applied: raise runner.RunnerError('fixture status unavailable')
            return {'TCP':{},'Web':{}}
        with mock.patch.object(self.manager,'origins',return_value=('wss://node.tail.ts.net:8443','https://node.tail.ts.net:10000')), mock.patch.object(self.manager,'ts_json',side_effect=status), mock.patch.object(self.manager,'tailscale',return_value='/fake/tailscale'), mock.patch.object(self.manager,'build_shell'), mock.patch.object(self.manager,'start_worker'), mock.patch.object(self.manager,'wait_ready'), mock.patch.object(runner.socket,'socket'), mock.patch.object(runner,'run_quiet',side_effect=command):
            with self.assertRaises(runner.RunnerError): self.manager.up()
        path=self.manager.state/'serve.json'
        self.assertTrue(path.exists(), 'status 실패 전에 복구 가능한 pending 기록이 있어야 한다')
        record=json.loads(runner.read_private(path))
        self.assertTrue(record['ports']['8443']['pending'])

    def test_existing_serve_journal_is_not_overwritten_by_new_up(self):
        path=self.manager.state/'serve.json'
        runner.write_private(path,b'{"binary":"/fake/tailscale","ports":{}}')
        with mock.patch.object(self.manager,'origins',side_effect=runner.RunnerError('external access')) as origins:
            with self.assertRaises(runner.RunnerError): self.manager.up()
            origins.assert_not_called()
        self.assertTrue(path.exists())

    def test_worker_publication_retries_a_partially_written_pid_record(self):
        nonce='c'*32
        child=mock.Mock(pid=12345); child.poll.return_value=None
        identity=['Tue Sep 8 12:00:00 2026',f'/usr/bin/python3 - {self.manager.script} _worker relay {nonce}']
        record={'pid':12345,'nonce':nonce,'role':'relay','identity':identity}
        with mock.patch.object(self.manager,'record',side_effect=[None,runner.RunnerError('publishing'),record]), mock.patch.object(runner.secrets,'token_hex',return_value=nonce), mock.patch.object(runner.subprocess,'Popen',return_value=child), mock.patch.object(runner,'identity',return_value=identity):
            self.assertIs(self.manager.start_worker('relay'),child)

    def test_published_supervisor_does_not_hide_a_dead_server_child(self):
        binary=self.manager.state/'target/debug/relay-server'
        binary.parent.mkdir(parents=True)
        binary.write_text('#!/bin/sh\nsleep 0.4\nexit 7\n'); binary.chmod(0o700)
        self.manager.credentials()
        child=self.manager.start_worker('relay')
        try:
            child.wait(timeout=3)
            with self.assertRaises(runner.RunnerError): self.manager.wait_ready('relay',timeout=0.1)
        finally:
            if child.poll() is None: child.terminate(); child.wait(timeout=5)

    def test_live_supervisor_without_a_bound_port_is_not_ready(self):
        binary=self.manager.state/'target/debug/relay-server'
        binary.parent.mkdir(parents=True)
        binary.write_text('#!/bin/sh\nexec sleep 60\n'); binary.chmod(0o700)
        self.manager.credentials()
        child=self.manager.start_worker('relay')
        try:
            with mock.patch.object(runner.socket,'create_connection',side_effect=ConnectionRefusedError()):
                with self.assertRaises(runner.RunnerError): self.manager.wait_ready('relay',timeout=0.1)
        finally:
            self.manager.stop_owned('relay'); child.wait(timeout=5)

    def test_another_process_listener_does_not_make_our_worker_ready(self):
        with mock.patch.object(self.manager,'verify_worker',return_value={'child_pid':12345}), mock.patch.object(runner.socket,'create_connection'), mock.patch.object(runner,'capture',return_value=b'p12345\nn127.0.0.1:1\n'):
            with self.assertRaises(runner.RunnerError): self.manager.wait_ready('relay',timeout=0.05)

    def test_env_cli_prints_no_credential_or_credential_prefix(self):
        binary = self.root/'tailscale-fixture'
        binary.write_text('#!/bin/sh\nprintf \'%s\\n\' \'{"Self":{"DNSName":"node.tail.ts.net."}}\'\n')
        binary.chmod(0o700)
        values = self.manager.credentials()
        result = subprocess.run(['/bin/sh',str(self.script),'env'], env=dict(os.environ, TAILSCALE_BIN=str(binary)), capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        output = result.stdout + result.stderr
        for secret in values.values():
            self.assertNotIn(secret, output)
            self.assertNotIn(secret[:8], output)
        self.assertIn('wss://node.tail.ts.net:8443', output)

    def test_app_requires_opt_in_and_contains_no_termination_call(self):
        # app 명령 자체는 실행하지 않는다. AST로 실행 옵션과 종료 호출 부재만 검증한다.
        tree = ast.parse(SOURCE)
        cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name=='Runner')
        app = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name=='app')
        calls = [n for n in ast.walk(app) if isinstance(n, ast.Call)]
        self.assertFalse(any(isinstance(n.func, ast.Attribute) and n.func.attr in ('kill','killpg','terminate') for n in calls))
        self.assertIsInstance(app.body[0], ast.If)
        self.assertIn('--launch-app', ast.unparse(app.body[0]))
        self.assertNotIn('pkill', SOURCE)
        self.assertNotIn('shell=True', SOURCE)

if __name__ == '__main__':
    unittest.main()
