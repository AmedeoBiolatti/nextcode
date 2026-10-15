"""Private-display UI automation for the unmodified NextCode GUI (Linux).

The HTTP contract deliberately contains only platform operations. No editor
widgets, labels, key bindings, or feature-specific behavior belong here.
"""
import argparse
import base64
import hmac
import json
import os
import secrets
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer


def write_script(path, body):
    with open(path, 'w') as script:
        script.write('#!/bin/sh\n' + body)
    os.chmod(path, 0o755)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--port', type=int, default=0)
    parser.add_argument('--width', type=int, default=1440)
    parser.add_argument('--height', type=int, default=900)
    parser.add_argument('--state-dir', help='data dir kept across sessions (default: private)')
    parser.add_argument('project', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.project[:1] == ['--']:
        args.project.pop(0)

    def terminate(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, terminate)
    if not sys.platform.startswith('linux'):
        parser.error('UI automation currently requires Linux (including WSL)')
    for tool in ('Xvfb', 'xdotool', 'import', 'xclip', 'xauth'):
        if not shutil.which(tool):
            parser.error(f'missing {tool}; install xvfb xdotool imagemagick xclip xauth')
    if not (320 <= args.width <= 8192 and 240 <= args.height <= 8192):
        parser.error('display size must be between 320x240 and 8192x8192')
    if not 0 <= args.port <= 65535:
        parser.error('invalid port')

    children = []
    server = None
    env = os.environ.copy()
    token = secrets.token_urlsafe(32)

    def spawn(argv, **kwargs):
        child = subprocess.Popen(argv, env=env, start_new_session=True, **kwargs)
        children.append(child)
        return child

    def run(argv, **kwargs):
        result = subprocess.run(argv, env=env, capture_output=True, timeout=10, **kwargs)
        if result.returncode:
            raise ValueError(result.stderr.decode(errors='replace').strip() or 'platform command failed')
        return result.stdout

    def number(request, key, low, high):
        value = request[key]
        if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
            raise ValueError(f'{key} must be an integer between {low} and {high}')
        return str(value)

    def string(request, key):
        value = request[key]
        if not isinstance(value, str) or '\0' in value:
            raise ValueError(f'{key} must be a string without NUL')
        return value

    try:
        # Xvfb allocates an unused display atomically; never connect to a user's
        # existing display. The private directory protects its authority cookie.
        with tempfile.TemporaryDirectory(prefix='next-code-ui-') as directory:
            authority = os.path.join(directory, 'Xauthority')
            env['XAUTHORITY'] = authority
            cookie = secrets.token_hex(16)
            run(['xauth', '-f', authority, 'add', ':0', '.', cookie])
            # WSLg mounts /tmp/.X11-unix read-only, so Xvfb cannot create its
            # socket there and `-displayfd` alone rejects every display. Then
            # listen only on the abstract socket, starting well above the
            # displays WSLg and users take; Xvfb's lock file in /tmp refuses
            # numbers already in use.
            if os.access('/tmp/.X11-unix', os.W_OK):
                candidates, extra = [None], []
            else:
                candidates, extra = range(100, 200), ['-nolisten', 'unix']
            display_number = None
            for candidate in candidates:
                read_fd, write_fd = os.pipe()
                try:
                    display = spawn(['Xvfb', *([f':{candidate}'] if candidate is not None else []),
                                     '-displayfd', str(write_fd), '-screen', '0',
                                     f'{args.width}x{args.height}x24', '-nolisten', 'tcp', *extra,
                                     '-auth', authority],
                                    pass_fds=(write_fd,), stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL)
                finally:
                    os.close(write_fd)
                import select
                try:
                    if not select.select([read_fd], [], [], 15)[0]:
                        raise RuntimeError('Xvfb did not become ready within 15 seconds')
                    allocated = os.read(read_fd, 32).decode().strip()
                finally:
                    os.close(read_fd)
                if allocated.isdecimal() and display.poll() is None:
                    display_number = allocated
                    break
                display.wait()
                children.remove(display)
            if display_number is None:
                raise RuntimeError('Xvfb failed to allocate a display')
            env['DISPLAY'] = ':' + display_number
            run(['xauth', '-f', authority, 'add', env['DISPLAY'], '.', cookie])
            env.pop('WAYLAND_DISPLAY', None)
            env['WINIT_UNIX_BACKEND'] = 'x11'
            # Keep automated edits to preferences, recent projects and the
            # per-project session separate from the user's. `--state-dir`
            # keeps the data (sessions, recents) across automation runs.
            env['XDG_CONFIG_HOME'] = os.path.join(directory, 'config')
            env['XDG_CACHE_HOME'] = os.path.join(directory, 'cache')
            env['XDG_DATA_HOME'] = (os.path.abspath(args.state_dir) if args.state_dir
                                    else os.path.join(directory, 'data'))
            # Nothing the editor opens may reach the user's desktop: without
            # a session bus the desktop portal is unreachable, and the
            # fallbacks (`powershell.exe` on WSL, `xdg-open`, `gio`) are
            # stand-ins that record the target for the `opened` command.
            env.pop('DBUS_SESSION_BUS_ADDRESS', None)
            opened_log = os.path.join(directory, 'opened.log')
            open(opened_log, 'w').close()
            shims = os.path.join(directory, 'bin')
            os.mkdir(shims)
            record = f'printf "%s\\n" "$target" >> {shlex.quote(opened_log)}'
            for name in ('xdg-open', 'gio', 'wslview', 'sensible-browser', 'x-www-browser'):
                write_script(os.path.join(shims, name),
                             f'target="$1"; [ "$1" = open ] && target="$2"\n{record}\n')
            real_powershell = shutil.which('powershell.exe', path=env.get('PATH'))
            write_script(os.path.join(shims, 'powershell.exe'),
                         'if [ -n "$OPEN_RS_TARGET" ]; then target="$OPEN_RS_TARGET"; '
                         f'{record}; exit 0; fi\n'
                         + (f'exec {shlex.quote(real_powershell)} "$@"\n' if real_powershell else 'exit 127\n'))
            env['PATH'] = shims + os.pathsep + env.get('PATH', '')
            env['BROWSER'] = os.path.join(shims, 'xdg-open')
            app = spawn([args.binary, *args.project], stdout=sys.stderr)
            deadline = time.monotonic() + 60
            window = None
            while time.monotonic() < deadline:
                if app.poll() is not None:
                    raise RuntimeError(f'NextCode exited during startup ({app.returncode})')
                try:
                    windows = run(['xdotool', 'search', '--onlyvisible', '--pid', str(app.pid)]).split()
                    if windows:
                        window = windows[0].decode()
                        break
                except ValueError:
                    pass
                time.sleep(.1)
            if window is None:
                raise RuntimeError('NextCode did not create a visible window within 60 seconds')
            # The editor centers its window; the display is the whole screen.
            run(['xdotool', 'windowmove', '--sync', window, '0', '0'])
            run(['xdotool', 'windowsize', '--sync', window, str(args.width), str(args.height)])
            run(['xdotool', 'windowfocus', '--sync', window])

            def execute(request):
                cmd = request.get('cmd')
                if cmd == 'info':
                    return {'pid': app.pid, 'window': window, 'width': args.width,
                            'height': args.height, 'coordinates': 'display pixels',
                            'commands': ['info', 'key_down', 'key_up', 'key', 'text',
                                         'mouse_move', 'mouse_down', 'mouse_up', 'click',
                                         'scroll', 'screenshot', 'wait', 'opened', 'shutdown']}
                if cmd in ('key_down', 'key_up', 'key'):
                    verb = {'key_down': 'keydown', 'key_up': 'keyup', 'key': 'key'}[cmd]
                    run(['xdotool', verb, '--', string(request, 'key')])
                elif cmd == 'text':
                    # Clipboard paste preserves Unicode and line breaks. Keep
                    # xclip alive only for this isolated display's lifetime.
                    child = spawn(['xclip', '-selection', 'clipboard', '-in', '-quiet'],
                                  stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL)
                    child.stdin.write(string(request, 'text').encode())
                    child.stdin.close()
                    run(['xdotool', 'key', '--clearmodifiers', 'ctrl+v'])
                elif cmd == 'mouse_move':
                    run(['xdotool', 'mousemove', '--sync',
                         number(request, 'x', 0, args.width - 1),
                         number(request, 'y', 0, args.height - 1)])
                elif cmd in ('mouse_down', 'mouse_up', 'click'):
                    button = number(request, 'button', 1, 255)
                    verb = {'mouse_down': 'mousedown', 'mouse_up': 'mouseup', 'click': 'click'}[cmd]
                    options = ['--repeat', number({'count': request.get('count', 1)}, 'count', 1, 100)] if cmd == 'click' else []
                    run(['xdotool', verb, *options, button])
                elif cmd == 'scroll':
                    direction = request.get('direction')
                    buttons = {'up': '4', 'down': '5', 'left': '6', 'right': '7'}
                    if direction not in buttons:
                        raise ValueError('direction must be up, down, left, or right')
                    run(['xdotool', 'click', '--repeat',
                         number({'count': request.get('count', 1)}, 'count', 1, 100), buttons[direction]])
                elif cmd == 'screenshot':
                    png = run(['import', '-window', 'root', 'png:-'])
                    return {'mime_type': 'image/png', 'base64': base64.b64encode(png).decode(),
                            'width': args.width, 'height': args.height}
                elif cmd == 'wait':
                    time.sleep(int(number(request, 'ms', 0, 10000)) / 1000)
                elif cmd == 'opened':
                    # URLs and files the editor asked the desktop to open.
                    with open(opened_log) as log:
                        return log.read().splitlines()
                elif cmd == 'shutdown':
                    threading.Thread(target=server.shutdown, daemon=True).start()
                else:
                    raise ValueError('unknown cmd; use info for supported commands')
                return None

            class Handler(BaseHTTPRequestHandler):
                def log_message(self, *_):
                    pass

                def setup(self):
                    super().setup()
                    self.connection.settimeout(15)

                def do_POST(self):
                    if not hmac.compare_digest(self.headers.get('Authorization', ''), 'Bearer ' + token):
                        self.send_error(401)
                        return
                    if self.path != '/command':
                        self.send_error(404)
                        return
                    request = {}
                    try:
                        length = int(self.headers.get('Content-Length', '0'))
                        if not 0 < length <= 4 * 1024 * 1024:
                            raise ValueError('body must be 1 byte to 4 MiB')
                        request = json.loads(self.rfile.read(length))
                        if not isinstance(request, dict):
                            request = {}
                            raise ValueError('body must be a JSON object')
                        if app.poll() is not None or display.poll() is not None:
                            raise ValueError('application or display has exited')
                        response = {'id': request.get('id'), 'ok': True, 'result': execute(request)}
                    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
                        response = {'id': request.get('id'), 'ok': False, 'error': str(error)}
                    payload = json.dumps(response).encode()
                    self.send_response(200)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)

            # Serial execution keeps held input and screenshot ordering explicit.
            server = HTTPServer(('127.0.0.1', args.port), Handler)
            server.timeout = 1
            print(json.dumps({'event': 'ready', 'url': f'http://127.0.0.1:{server.server_port}/command',
                              'token': token, 'pid': app.pid}), flush=True)
            def monitor():
                while app.poll() is None and display.poll() is None:
                    time.sleep(.5)
                server.shutdown()

            threading.Thread(target=monitor, daemon=True).start()
            server.serve_forever()
    finally:
        if server:
            server.server_close()
        # Newest first, one at a time: the editor closes its windows on
        # SIGTERM (stopping dev containers it started, shutting down
        # language servers), which needs the display still running.
        for child in reversed(children):
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                continue
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()


if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        pass
