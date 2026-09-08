#!/bin/sh
# 개발용 Relay runner. 자격증명은 출력하지 않으며 기존 앱을 종료하지 않는다.
set -eu
exec python3 - "$0" "$@" <<'PY_RELAY_RUNNER'
import os
import re
import stat
from pathlib import Path

class RunnerError(Exception):
    pass

def port(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]{1,5}", value) or not 1 <= int(value) <= 65535:
        raise RunnerError('포트는 1..65535 정수여야 한다')
    return int(value)

def host(value):
    if not isinstance(value, str) or len(value) > 254:
        raise RunnerError('tailnet DNS 이름이 잘못되었다')
    value = value.rstrip('.').lower()
    labels = value.split('.')
    if len(value) > 253 or not value.endswith('.ts.net') or any(not re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?', label) for label in labels):
        raise RunnerError('유효한 ts.net DNS 이름만 허용한다')
    return value

def safe_path(value):
    value = str(value)
    if not value or len(value.encode()) > 4096 or any(ord(c) < 32 or c in "'`$\\" for c in value):
        raise RunnerError('경로가 너무 길거나 허용하지 않는 문자가 있다')
    return Path(value)

def parse_env(data):
    if len(data) > 4096:
        raise RunnerError('env 파일 크기 상한 초과')
    try:
        lines = data.decode('utf-8').splitlines()
    except UnicodeError:
        raise RunnerError('env 형식이 잘못되었다') from None
    fields = {}
    expected = {'DEPPY_RELAY_DEV_ROUTE': 32, 'DEPPY_RELAY_DEV_ADMISSION': 64}
    for line in lines:
        if not line or line.startswith('#'):
            continue
        key, sep, value = line.partition('=')
        if not sep or key not in expected or key in fields or not re.fullmatch('[0-9a-fA-F]{%d}' % expected.get(key, 0), value):
            raise RunnerError('env는 중복 없는 두 hex 필드만 허용한다')
        fields[key] = value.lower()
    if set(fields) != set(expected):
        raise RunnerError('env 필드가 누락되었다')
    return fields

def read_private(path, limit=4096):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600 or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_size > limit:
                raise RunnerError('private 파일은 소유자 일치/regular/0600/단일 링크/크기 상한이 필요하다')
            data = stream.read(limit + 1)
            if len(data) > limit:
                raise RunnerError('private 파일 크기 상한 초과')
            return data
    except OSError:
        raise RunnerError('private 파일을 안전하게 읽을 수 없다') from None

def owned_process(record, observed, script):
    if not isinstance(record, dict) or not isinstance(observed, list) or len(observed) != 2:
        return False
    pid, nonce, role = record.get('pid'), record.get('nonce'), record.get('role')
    if type(pid) is not int or pid <= 1 or role not in ('relay', 'shell') or not isinstance(nonce, str) or not re.fullmatch('[0-9a-f]{32}', nonce):
        return False
    return record.get('identity') == observed and observed[1].endswith(f' - {script} _worker {role} {nonce}')

import contextlib
import fcntl
import hashlib
import json
import secrets
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import time


def write_private(path, data):
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError:
        raise RunnerError('private 파일을 새로 만들 수 없다') from None


def private_dir(path):
    try:
        path.mkdir(mode=0o700)
    except FileExistsError:
        pass
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o700 or info.st_uid != os.getuid():
        raise RunnerError('상태 디렉터리는 소유자 일치/0700이어야 한다')


def stop_child(child):
    # wait 전까지 미회수 child PID는 재사용되지 않는다. 다른 프로세스는 신호하지 않는다.
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=3)


def capture(argv, timeout=10, limit=1024 * 1024):
    child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    output = bytearray()
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            deadline = time.monotonic() + timeout
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise RunnerError('외부 명령 응답 시간 초과')
                block = os.read(child.stdout.fileno(), 16384)
                if not block:
                    break
                output.extend(block)
                if len(output) > limit:
                    raise RunnerError('외부 명령 응답 크기 상한 초과')
            if child.wait(timeout=max(0.1, deadline - time.monotonic())) != 0:
                raise RunnerError('외부 명령 실행 실패')
            return bytes(output)
    finally:
        stop_child(child)
        child.stdout.close()


def identity(pid):
    if type(pid) is not int or pid <= 1:
        return None
    try:
        raw = capture(['/bin/ps', '-ww', '-p', str(pid), '-o', 'lstart=', '-o', 'command='], 2, 8192)
        fields = raw.decode('utf-8').strip().split(None, 5)
        return [' '.join(fields[:5]), fields[5]] if len(fields) == 6 else None
    except (RunnerError, UnicodeError):
        return None


def run_quiet(argv, env, timeout):
    # WNOWAIT로 leader PID를 회수하지 않은 채 group을 정리해 PID 재사용을 막는다.
    if not hasattr(os, 'waitid') or not hasattr(os, 'WNOWAIT'):
        raise RunnerError('안전한 process group 정리에 waitid/WNOWAIT가 필요하다')
    child = subprocess.Popen(argv, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             stdin=subprocess.DEVNULL, start_new_session=True)
    try:
        deadline = time.monotonic() + timeout
        while True:
            result = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
            if result is not None:
                if result.si_code != os.CLD_EXITED or result.si_status != 0:
                    raise RunnerError('외부 명령 실행 실패')
                break
            if time.monotonic() >= deadline:
                raise RunnerError('외부 명령 실행 시간 초과')
            time.sleep(0.02)
    except BaseException:
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(child.pid, sig)
            except ProcessLookupError:
                break
            except PermissionError:
                # macOS는 종료한 zombie group에도 EPERM을 줄 수 있다. 성공으로 숨기지 않는다.
                raise RunnerError('자체 process group 종료 확인에 OS 권한 오류가 있다') from None
            if sig == signal.SIGTERM:
                time.sleep(0.1)
        raise
    finally:
        child.wait(timeout=3)


class Runner:
    def __init__(self, script):
        self.script = safe_path(script).resolve()
        self.root = safe_path(self.script.parent.parent)
        self.state = self.root / '.relay-dev'
        private_dir(self.state)
        self.env_file = self.root / '.relay-dev.env'
        self.relay_port = port(os.environ.get('RELAY_PORT', '8443'))
        self.shell_port = port(os.environ.get('SHELL_PORT', '10000'))
        local = os.environ.get('RELAY_LOCAL', '127.0.0.1:9443')
        if not local.startswith('127.0.0.1:'):
            raise RunnerError('Relay bind는 127.0.0.1 loopback만 허용한다')
        self.local_port = port(local[len('127.0.0.1:'):])
        self.shell_local_port = port(os.environ.get('SHELL_LOCAL_PORT', '10080'))
        if self.relay_port == self.shell_port or self.local_port == self.shell_local_port:
            raise RunnerError('Relay와 shell의 포트가 겹친다')
        self.local = f'127.0.0.1:{self.local_port}'
        self.workers = []

    @contextlib.contextmanager
    def locked(self):
        fd = os.open(self.state/'lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1:
                raise RunnerError('lock 파일 identity가 잘못되었다')
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise RunnerError('다른 runner 작업이 진행 중이다') from None
            yield
        finally:
            os.close(fd)

    def credentials(self):
        if not os.path.lexists(self.env_file):
            values = {'DEPPY_RELAY_DEV_ROUTE': secrets.token_hex(16),
                      'DEPPY_RELAY_DEV_ADMISSION': secrets.token_hex(32)}
            write_private(self.env_file, ''.join(f'{k}={v}\n' for k, v in values.items()).encode())
        return parse_env(read_private(self.env_file))

    def record(self, role):
        path = self.state/f'{role}.pid'
        if not os.path.lexists(path):
            return None
        try:
            record = json.loads(read_private(path))
            if not isinstance(record, dict) or record.get('role') != role:
                raise ValueError()
            return record
        except (ValueError, TypeError):
            raise RunnerError('PID 기록 형식이 잘못되었다') from None

    def stop_owned(self, role):
        record = self.record(role)
        if record is None:
            return False
        observed = identity(record.get('pid'))
        if observed is None:
            # 존재하지 않는 PID만 기록을 지운다. ps 실패는 kill(0)으로 생존을 재확인한다.
            try:
                pid = record.get('pid')
                if type(pid) is not int or pid <= 1:
                    raise RunnerError('PID 기록이 잘못되었다')
                os.kill(pid, 0)
            except ProcessLookupError:
                (self.state/f'{role}.pid').unlink()
                return False
            raise RunnerError('프로세스 identity를 확인할 수 없어 종료하지 않는다')
        if not owned_process(record, observed, str(self.script)):
            raise RunnerError('PID 소유권 불일치: 해당 프로세스를 종료하지 않는다')
        os.kill(record['pid'], signal.SIGTERM)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if not os.path.lexists(self.state/f'{role}.pid'):
                return True
            time.sleep(0.05)
        raise RunnerError('자체 supervisor 종료 대기 시간 초과')

    def start_worker(self, role):
        if role not in ('relay', 'shell'):
            raise RunnerError('알 수 없는 worker')
        if self.record(role) is not None:
            raise RunnerError('기존 PID 기록은 down으로 먼저 확인해야 한다')
        nonce = secrets.token_hex(16)
        child = subprocess.Popen(['/bin/sh', str(self.script), '_worker', role, nonce],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 stdin=subprocess.DEVNULL, start_new_session=True)
        self.workers.append(child)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and child.poll() is None:
            try:
                record = self.record(role)
            except RunnerError:
                # O_EXCL 파일이 생성되고 JSON 쓰기가 끝나기 전인 짧은 공개 구간을 기다린다.
                time.sleep(0.05)
                continue
            if record and record.get('nonce') == nonce and owned_process(record, identity(child.pid), str(self.script)):
                return child
            time.sleep(0.05)
        stop_child(child)
        raise RunnerError('자체 worker를 시작하지 못했다')

    def worker(self, role, nonce):
        if role not in ('relay', 'shell') or not re.fullmatch('[0-9a-f]{32}', nonce):
            raise RunnerError('worker 인자가 잘못되었다')
        env = dict(os.environ)
        env.update({'DEPPY_RELAY_BIND': self.local})
        if role == 'relay':
            values = self.credentials()
            env['DEPPY_RELAY_ROUTES'] = values['DEPPY_RELAY_DEV_ROUTE'] + ':' + values['DEPPY_RELAY_DEV_ADMISSION']
            argv = [str(self.state/'target/debug/relay-server')]
        else:
            argv = [sys.executable, '-m', 'http.server', str(self.shell_local_port), '--bind', '127.0.0.1',
                    '--directory', str(self.state/'serve')]
        path = self.state/f'{role}.pid'
        child = None
        stopping = False
        def terminate(_signal, _frame):
            nonlocal stopping
            stopping = True
            if child is not None:
                raise SystemExit(0)
        signal.signal(signal.SIGTERM, terminate)
        signal.signal(signal.SIGINT, terminate)
        try:
            # 생성 중 신호는 flag로 보류한다. child에 차단된 signal mask를 물려주지 않는다.
            child = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if stopping:
                raise SystemExit(0)
            observed = identity(os.getpid())
            record = {'pid': os.getpid(), 'role': role, 'nonce': nonce, 'identity': observed,
                      'child_pid': child.pid, 'child_identity': identity(child.pid)}
            if record['child_identity'] is None or child.poll() is not None:
                raise RunnerError('서버 child가 시작 중 종료되었다')
            if not owned_process(record, observed, str(self.script)):
                raise RunnerError('supervisor identity 확인 실패')
            write_private(path, json.dumps(record).encode())
            child.wait()
        finally:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            if child is not None:
                stop_child(child)
            if os.path.lexists(path):
                record = self.record(role)
                if record.get('nonce') == nonce:
                    path.unlink()

    def tailscale(self):
        value = os.environ.get('TAILSCALE_BIN') or shutil.which('tailscale') or '/Applications/Tailscale.app/Contents/MacOS/Tailscale'
        binary = safe_path(value).resolve()
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RunnerError('Tailscale CLI가 없다: 외부 검증 BLOCKED')
        return str(binary)

    def ts_json(self, *args):
        try:
            value = json.loads(capture([self.tailscale(), *args]))
            if not isinstance(value, dict):
                raise ValueError()
            return value
        except (ValueError, TypeError):
            raise RunnerError('Tailscale JSON 응답 형식 오류') from None

    def origins(self):
        node = host(self.ts_json('status', '--json').get('Self', {}).get('DNSName'))
        return f'wss://{node}:{self.relay_port}', f'https://{node}:{self.shell_port}'

    @staticmethod
    def serve_part(config, number):
        tcp, web = config.get('TCP', {}), config.get('Web', {})
        if not isinstance(tcp, dict) or not isinstance(web, dict):
            raise RunnerError('serve 설정 형식 오류')
        return {'TCP': tcp.get(str(number)), 'Web': {k: v for k, v in web.items() if k.endswith(f':{number}')}}

    def serve_record(self, entries):
        path = self.state/'serve.json'
        temporary = self.state/f'serve-{secrets.token_hex(8)}.tmp'
        write_private(temporary, json.dumps({'binary': self.tailscale(), 'ports': entries}).encode())
        os.replace(temporary, path)

    def down_serves(self):
        path = self.state/'serve.json'
        if not os.path.lexists(path):
            return
        record = json.loads(read_private(path, 65536))
        if record.get('binary') != self.tailscale() or not isinstance(record.get('ports'), dict):
            raise RunnerError('serve 소유권 기록 불일치')
        entries = record['ports']
        errors = []
        for number in list(entries):
            try:
                port(number)
                current = self.serve_part(self.ts_json('serve', 'status', '--json'), number)
                entry = entries[number]
                expected = entry.get('expected', entry)
                empty = current['TCP'] is None and not current['Web']
                if current != expected and not (entry.get('pending') and empty):
                    raise RunnerError('serve 설정이 외부에서 바뀌어 회수하지 않는다')
                if not empty:
                    run_quiet([self.tailscale(), 'serve', '--https='+number, 'off'], os.environ, 15)
                del entries[number]
                self.serve_record(entries)
            except (RunnerError, OSError, ValueError, TypeError):
                errors.append(number)
        if not entries:
            path.unlink()
        if errors:
            raise RunnerError('일부 serve 소유권/상태가 미확인이다. pending 기록을 보존했다')

    def build_shell(self, relay, shell):
        env = {k: v for k, v in os.environ.items() if not k.startswith('DEPPY_RELAY_')}
        temporary = self.state/'tmp'
        private_dir(temporary)
        env.update({'SHELL_ORIGIN': shell, 'RELAY_ORIGIN': relay, 'RELAY_SHELL_DIST': str(self.state/'dist'),
                    'TMPDIR': str(temporary), 'CARGO_TARGET_DIR': str(self.state/'target'), 'CARGO_BUILD_JOBS': '2'})
        run_quiet(['cargo', 'build', '--locked', '-p', 'relay-server', '--bin', 'relay-server', '--manifest-path', str(self.root/'Cargo.toml')], env, 600)
        run_quiet(['/bin/sh', str(self.root/'web/relay-shell/build.sh')], env, 120)
        manifest_path = self.state/'dist/relay-shell.manifest.json'
        manifest = json.loads(read_private(manifest_path, 65536))
        digest = manifest.get('archive_sha256', '')
        name = manifest.get('archive', '')
        if not re.fullmatch('[0-9a-f]{64}', digest) or name != f'relay-shell-{digest}.tar.gz':
            raise RunnerError('shell archive 경로/해시 형식 오류')
        archive = self.state/'dist'/name
        archive_bytes = read_private(archive, 16 * 1024 * 1024)
        if hashlib.sha256(archive_bytes).hexdigest() != digest:
            raise RunnerError('shell archive 검증 실패')
        destination = self.state/'serve'
        allowed = {'index.html', 'relay-shell.js', 'relay-terminal.js', 'relay-crypto.js', 'relay-shell.css', 'sw.js', 'manifest.webmanifest', 'mobile-theme.css'}
        previous = []
        if os.path.lexists(destination):
            private_dir(destination)
            previous = list(destination.iterdir())
            if len(previous) > len(allowed) or any(item.name not in allowed for item in previous):
                raise RunnerError('소유하지 않은 serve 자산을 제거하지 않는다')
            for item in previous:
                read_private(item, 16 * 1024 * 1024)
        stage = self.state/('serve-stage-'+secrets.token_hex(16))
        private_dir(stage)
        try:
            seen = set()
            import io
            with tarfile.open(fileobj=io.BytesIO(archive_bytes), mode='r:gz') as tar:
                total = 0
                for count, item in enumerate(tar, 1):
                    if count > 9:
                        raise RunnerError('shell archive 항목 수 상한 초과')
                    if item.name.rstrip('/') == 'relay-shell' and item.isdir():
                        continue
                    name = item.name.removeprefix('relay-shell/')
                    total += item.size
                    if item.name != 'relay-shell/'+name or name not in allowed or name in seen or not item.isfile() or item.size < 0 or total > 16 * 1024 * 1024:
                        raise RunnerError('shell archive 항목/크기 검증 실패')
                    seen.add(name)
                    with tar.extractfile(item) as data:
                        write_private(stage/name, data.read(item.size + 1))
                if seen != allowed:
                    raise RunnerError('shell archive 자산 누락')
            # 검증된 새 자산만 게시한다. 실패하면 이전 디렉터리를 원위치에 둔다.
            backup = self.state/('serve-old-'+secrets.token_hex(16))
            existed = destination.exists()
            if existed:
                os.rename(destination, backup)
            try:
                os.rename(stage, destination)
            except BaseException:
                if existed:
                    os.rename(backup, destination)
                raise
            if existed:
                for item in previous:
                    (backup/item.name).unlink()
                backup.rmdir()
        finally:
            if stage.exists():
                for item in stage.iterdir():
                    item.unlink()
                stage.rmdir()

    def verify_worker(self, role):
        record = self.record(role)
        if not record or not owned_process(record, identity(record.get('pid')), str(self.script)):
            raise RunnerError('자체 supervisor가 실행 중이지 않다')
        observed = identity(record.get('child_pid'))
        if observed is None or observed != record.get('child_identity'):
            raise RunnerError('서버 child가 종료되거나 교체되었다')
        return record

    def wait_ready(self, role, timeout=5):
        number = self.local_port if role == 'relay' else self.shell_local_port
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            record = self.verify_worker(role)
            try:
                # 연결 성공만으로 다른 프로세스의 listener를 우리 서버로 오인하지 않는다.
                lsof = shutil.which('lsof') or '/usr/sbin/lsof'
                listeners = capture([lsof, '-nP', '-a', '-p', str(record['child_pid']),
                                     '-iTCP:'+str(number), '-sTCP:LISTEN', '-Fn'], 2, 8192)
                if f'n127.0.0.1:{number}'.encode() not in listeners.splitlines():
                    time.sleep(0.05)
                    continue
                with socket.create_connection(('127.0.0.1', number), timeout=0.2):
                    self.verify_worker(role)
                    return
            except (OSError, RunnerError):
                time.sleep(0.05)
        raise RunnerError('로컬 서버 포트 준비 시간 초과')

    def up(self):
        if os.path.lexists(self.state/'serve.json'):
            raise RunnerError('기존 serve 기록은 down으로 먼저 확인해야 한다')
        self.credentials()
        relay, shell = self.origins()
        config = self.ts_json('serve', 'status', '--json')
        for number in (self.relay_port, self.shell_port):
            part = self.serve_part(config, number)
            if part['TCP'] is not None or part['Web']:
                raise RunnerError('이미 사용 중인 Tailscale 포트를 덮어쓰지 않는다')
        for number in (self.local_port, self.shell_local_port):
            with socket.socket() as listener:
                listener.bind(('127.0.0.1', number))
        for role in ('relay', 'shell'):
            if self.record(role) is not None:
                raise RunnerError('기존 runner는 down으로 먼저 확인해야 한다')
        self.build_shell(relay, shell)
        entries = {}
        try:
            for role in ('relay', 'shell'):
                self.start_worker(role)
                self.wait_ready(role)
            for number, target in [(self.relay_port, self.local), (self.shell_port, f'127.0.0.1:{self.shell_local_port}')]:
                # 외부 writer와 CAS는 제공되지 않으므로 전용 포트를 쓰고 적용 직전 재확인한다.
                current = self.serve_part(self.ts_json('serve', 'status', '--json'), number)
                if current['TCP'] is not None or current['Web']:
                    raise RunnerError('빌드 이후 사용 중이 된 serve 포트를 덮어쓰지 않는다')
                node = relay.removeprefix('wss://').rsplit(':', 1)[0]
                expected = {'TCP': {'HTTPS': True}, 'Web': {f'{node}:{number}': {'Handlers': {'/': {'Proxy': 'http://'+target}}}}}
                entries[str(number)] = {'expected': expected, 'pending': True}
                self.serve_record(entries)
                run_quiet([self.tailscale(), 'serve', '--bg', '--https='+str(number), target], os.environ, 15)
                actual = self.serve_part(self.ts_json('serve', 'status', '--json'), number)
                if actual != expected:
                    raise RunnerError('serve 적용 결과가 예상과 달라 pending으로 보존한다')
                entries[str(number)]['pending'] = False
                self.serve_record(entries)
            for role in ('relay', 'shell'):
                self.wait_ready(role)
        except BaseException:
            for child in self.workers:
                stop_child(child)
            # 성공 후 기록한 설정만 회수한다. 미확인 설정은 외부 소유권을 추정하지 않는다.
            self.down_serves()
            raise
        print('relay-dev: 로컬 프로세스와 serve 명령 완료; DNS/TLS/실기기/배포 검증은 BLOCKED')
        print('relay:', relay)
        print('shell:', shell)

    def down(self):
        errors = []
        for role in ('relay', 'shell'):
            try:
                self.stop_owned(role)
            except RunnerError:
                errors.append(role)
        self.down_serves()
        if errors:
            raise RunnerError('일부 PID 소유권을 확인할 수 없어 종료하지 않았다')
        print('relay-dev: 자체 프로세스와 확인된 serve 설정만 정리했다')

    def status(self):
        for role in ('relay', 'shell'):
            record = self.record(role)
            state = '정지' if record is None else ('자체 프로세스 실행 중' if owned_process(record, identity(record.get('pid')), str(self.script)) else '소유권 확인 필요')
            print(role+': '+state)
        print('외부 Tailscale/DNS/TLS/배포 검증: BLOCKED')

    def app(self, args):
        if args != ['--launch-app']:
            raise RunnerError('경고: app은 GUI를 빌드/새 실행한다. 기존 앱은 종료하지 않는다. 명시적 --launch-app이 필요하다')
        print('경고: GUI를 새로 실행하며 기존 앱은 그대로 유지한다.', file=sys.stderr)
        relay, shell = self.origins()
        env = dict(os.environ, **self.credentials())
        env.update({'DEPPY_RELAY_DEV_ENDPOINT': relay, 'DEPPY_RELAY_DEV_SHELL_ORIGIN': shell})
        os.execve('/bin/sh', ['/bin/sh', str(self.root/'scripts/dev-run.sh')], env)


def main(script, args):
    os.umask(0o077)
    manager = Runner(script)
    if len(args) == 3 and args[0] == '_worker':
        manager.worker(args[1], args[2])
        return
    command = args[0] if args else 'status'
    if command not in ('up', 'down', 'status', 'env', 'app') or (command != 'app' and len(args) > 1):
        raise RunnerError('명령: up | down | status | env | app --launch-app')
    with manager.locked():
        if command == 'app':
            manager.app(args[1:])
        elif command == 'env':
            manager.credentials()
            relay, shell = manager.origins()
            print('DEPPY_RELAY_DEV_ENDPOINT='+relay)
            print('DEPPY_RELAY_DEV_SHELL_ORIGIN='+shell)
            print('route/admission: private env 파일에 저장됨 (값 출력 안 함)')
        else:
            getattr(manager, command)()


if __name__ == '__main__':
    try:
        main(sys.argv[1], sys.argv[2:])
    except (RunnerError, OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError):
        # exception 원문에 외부 응답/환경값이 들어갈 수 있으므로 traceback은 출력하지 않는다.
        print('relay-dev: 작업 실패. 입력/권한/소유권 또는 외부 조건을 확인하라. app은 --launch-app 명시 필요.', file=sys.stderr)
        raise SystemExit(1)

PY_RELAY_RUNNER
