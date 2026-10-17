# next_code kernel bridge (protocol: crates/notebook/src/protocol.rs).
#
# Started as `python -u -c BOOTSTRAP` with the notebook's interpreter where
# the project lives; this script arrives as the first stdin line (a JSON
# string). Needs only the standard library and `jupyter_client` (plus
# `ipykernel` for the default kernel); Python >= 3.8. Keep it ASCII.
#
# Stdio: the real stdout fd is duplicated for the protocol and fd 1 then
# points at stderr, so nothing else (the kernel inherits fds; C libraries;
# stray prints) can corrupt protocol lines. Protocol lines are UTF-8 JSON,
# one per line, flushed each. Stdin carries one JSON request per line.
#
# Threads: a stdin reader thread parses requests onto a queue (EOF: shut
# the kernel down and exit 0). The main thread alone touches ZeroMQ: it
# polls the iopub/shell/control sockets (~50 ms) and drains the queue;
# every send happens there.
#
# Requests (`{"id", "op", ...}`, each answered by exactly one reply line):
# - start {kernel_name?, cwd}: a KernelManager for the kernelspec, else an
#   ipykernel in THIS interpreter (`sys.executable -m ipykernel_launcher`,
#   overriding any installed `python3` spec), started in `cwd` with stdin
#   from /dev/null, stdout to /dev/null and stderr inherited (the bridge's
#   stderr, which next_code keeps as diagnostics). The launch is jupyter_
#   client's non-independent one: JPY_PARENT_PID makes ipykernel's parent
#   poller exit the kernel when the bridge dies (even on SIGKILL). After
#   `wait_for_ready` the marimo helper is injected silently; the reply is
#   the `kernel_info_reply` content. Status `starting`, then `idle`.
# - execute {code, marimo?}: `execute_request` (store_history, no stdin);
#   iopub `status` busy/idle -> `status` events with `request`;
#   `execute_input` -> `execute_input` event; `stream`, `display_data`,
#   `execute_result`, `error`, `clear_output`, `update_display_data` ->
#   `output` events with the content unchanged. The reply (`{status,
#   execution_count, ename?, evalue?}`) comes once both the shell
#   `execute_reply` and the iopub idle for the request arrived. With
#   `marimo` {cell, defs, refs} the executed code is
#   `__nc_marimo__.run(cell, code, defs, refs)` (Python literals).
# - interrupt: `km.interrupt_kernel()`, reply null.
# - restart: status `restarting`, pending requests fail,
#   `km.restart_kernel(now=True)`, wait for ready, re-inject the helper,
#   status `idle`; reply: the new `kernel_info_reply` content.
# - shutdown: shuts the kernel down (a shutdown request, ~1 s grace, then a
#   kill), replies null and exits.
# - complete / inspect / is_complete: shell requests; reply: their content.
# - kernelspecs: `{name: {display_name, language, argv}}`.
# The kernel process is checked about every second; when it died: a `dead`
# event, status `dead`, and pending requests fail.
#
# The marimo helper `__nc_marimo__` (in the kernel's globals):
# `run(cell, code, defs, refs)` compiles `code` with the filename
# `<cell NAME>` registered in `linecache` (tracebacks show the source), runs
# every statement but a trailing expression in a fresh dict namespace
# holding the `refs` present in the kernel's `user_ns` (plus builtins),
# evaluates the trailing expression, publishes the public `defs` (no `_`
# prefix) into `user_ns` and deletes names this cell published on its
# previous run and no longer defines (on an error, everything it published
# before). Its own frames are hidden from tracebacks (`__tracebackhide__`,
# and the error's traceback starts at the cell's code). The return value is
# the last expression's value, which IPython displays as `execute_result`;
# values with marimo's `_mime_()` are wrapped in an object with
# `_repr_mimebundle_` ({mime: data, 'text/plain': ...}; marimo's markdown,
# whose `_mime_` is HTML labelled text/markdown, yields `text/html` and
# `text/markdown` with the source: `mo.md` is wrapped once marimo is
# imported so its results remember their markdown).

import json
import os
import queue
import subprocess
import sys
import threading
import time

POLL_SECONDS = 0.05
ALIVE_SECONDS = 1.0
READY_TIMEOUT = 60.0
REQUEST_TIMEOUT = 30.0
FORWARDED = (
    "stream",
    "display_data",
    "execute_result",
    "error",
    "clear_output",
    "update_display_data",
)

MARIMO_HELPER = r'''
def __nc_install_marimo_helper():
    import ast
    import builtins
    import inspect
    import linecache
    import sys

    from IPython import get_ipython

    class Rich(object):
        def __init__(self, bundle):
            self._bundle = bundle

        def _repr_mimebundle_(self, include=None, exclude=None):
            return self._bundle

        def __repr__(self):
            return self._bundle.get("text/plain", "")

    def markdown_of(value):
        text = getattr(value, "_markdown_text", None)
        if not isinstance(text, str):
            text = getattr(value, "__nc_markdown__", None)
        return text if isinstance(text, str) else None

    def rich(value):
        mime_of = getattr(value, "_mime_", None)
        if not callable(mime_of) or isinstance(value, type):
            return value
        try:
            mime, data = mime_of()
        except Exception:
            return value
        markdown = markdown_of(value)
        bundle = {}
        if markdown is not None:
            bundle["text/markdown"] = markdown
            bundle["text/html"] = data
        elif mime == "text/markdown" and data.lstrip().startswith("<"):
            bundle["text/html"] = data
        else:
            bundle[mime] = data
        if markdown is not None:
            bundle["text/plain"] = markdown
        elif mime == "text/plain":
            bundle["text/plain"] = data
        else:
            try:
                bundle["text/plain"] = repr(value)
            except Exception:
                bundle["text/plain"] = "<%s>" % type(value).__name__
        return Rich(bundle)

    def patch_marimo():
        marimo = sys.modules.get("marimo")
        md = getattr(marimo, "md", None)
        if md is None or getattr(md, "__nc_patched__", False):
            return

        def nc_md(text, *args, **kwargs):
            value = md(text, *args, **kwargs)
            if isinstance(text, str):
                try:
                    value.__nc_markdown__ = inspect.cleandoc(text)
                except Exception:
                    pass
            return value

        nc_md.__nc_patched__ = True
        nc_md.__wrapped__ = md
        nc_md.__doc__ = md.__doc__
        nc_md.__name__ = "md"
        try:
            marimo.md = nc_md
        except Exception:
            pass

    class CellSyntaxError(Exception):
        pass

    CellSyntaxError.__name__ = "SyntaxError"
    CellSyntaxError.__qualname__ = "SyntaxError"

    class Helper(object):
        def __init__(self):
            self._published = {}

        def run(self, cell, code, defs, refs):
            __tracebackhide__ = True
            user_ns = get_ipython().user_ns
            filename = "<cell %s>" % cell
            linecache.cache[filename] = (
                len(code),
                None,
                code.splitlines(True),
                filename,
            )
            previous = self._published.pop(cell, set())
            namespace = {"__builtins__": builtins, "__name__": "__main__"}
            for name in refs:
                if name in user_ns:
                    namespace[name] = user_ns[name]
            shell = get_ipython()
            try:
                tree = ast.parse(code, filename, "exec")
                last = None
                if tree.body and isinstance(tree.body[-1], ast.Expr):
                    last = ast.Expression(tree.body.pop().value)
                body = compile(tree, filename, "exec")
                if last is not None:
                    last = compile(last, filename, "eval")
            except (SyntaxError, ValueError) as error:
                for name in previous:
                    user_ns.pop(name, None)
                # IPython prints SyntaxErrors with the whole traceback
                # (helper frames included): report it as a plain error.
                stb = shell.SyntaxTB.structured_traceback(type(error), error, None)
                failure = CellSyntaxError(str(error))
                failure._render_traceback_ = lambda: stb
                raise failure from None
            try:
                patch_marimo()
                exec(body, namespace)
                value = None
                if last is not None:
                    patch_marimo()
                    value = eval(last, namespace)
            except BaseException as error:
                for name in previous:
                    user_ns.pop(name, None)
                if isinstance(error, SystemExit):
                    raise
                tb = error.__traceback__
                if tb is not None and tb.tb_frame.f_code is Helper.run.__code__:
                    tb = tb.tb_next
                try:
                    stb = shell.InteractiveTB.structured_traceback(
                        type(error), error, tb, tb_offset=0
                    )
                    error._render_traceback_ = lambda: stb
                except Exception:
                    pass
                raise
            published = set()
            for name in defs:
                if name.startswith("_") or name not in namespace:
                    continue
                user_ns[name] = namespace[name]
                published.add(name)
            for name in previous - published:
                user_ns.pop(name, None)
            self._published[cell] = published
            return rich(value)

    return Helper()


__nc_marimo__ = __nc_install_marimo_helper()
del __nc_install_marimo_helper
'''


def setup_stdio():
    """The protocol stream: a dup of the real stdout; fd 1 becomes stderr."""
    proto = os.fdopen(os.dup(1), "wb", buffering=0)
    try:
        sys.stdout.flush()
    except Exception:
        pass
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return proto


def json_safe(value):
    if isinstance(value, float):
        return value if value == value and value not in (float("inf"), float("-inf")) else None
    if isinstance(value, dict):
        return {str(k): json_safe(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [json_safe(v) for v in value]
    return value


class Writer(object):
    def __init__(self, stream):
        self._stream = stream
        self._lock = threading.Lock()

    def send(self, message):
        try:
            text = json.dumps(message, ensure_ascii=False, allow_nan=False, default=str)
        except ValueError:
            text = json.dumps(json_safe(message), ensure_ascii=False, default=str)
        data = (text + "\n").encode("utf-8", "replace")
        with self._lock:
            try:
                self._stream.write(data)
                self._stream.flush()
            except (OSError, ValueError):
                # next_code went away: nothing left to talk to.
                os._exit(0)

    def event(self, name, **fields):
        fields["event"] = name
        self.send(fields)

    def reply(self, request, result=None):
        self.send({"reply": request, "ok": True, "result": result})

    def fail(self, request, error):
        self.send({"reply": request, "ok": False, "error": str(error)})


def missing_modules():
    import importlib.util

    missing = []
    for name in ("ipykernel", "jupyter_client"):
        try:
            if importlib.util.find_spec(name) is None:
                missing.append(name)
        except Exception:
            missing.append(name)
    return missing


def read_stdin(requests):
    while True:
        try:
            line = sys.stdin.readline()
        except Exception:
            line = ""
        if not line:
            requests.put(None)
            return
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except ValueError as error:
            requests.put({"id": None, "op": "invalid", "error": str(error)})
            continue
        requests.put(request)


class Execution(object):
    def __init__(self, request):
        self.request = request
        self.reply = None
        self.idle = False


class Bridge(object):
    def __init__(self, out):
        self.out = out
        self.km = None
        self.kc = None
        self.poller = None
        self.language = None
        # msg_id -> request id of every execute (outputs of background
        # threads arrive under finished ones); bounded.
        self.requests = {}
        self.executions = {}  # msg_id -> Execution (unanswered)
        self.shell_requests = {}  # msg_id -> request id (complete, ...)
        self.last_alive_check = time.monotonic()
        self.stopping = False

    # -- main loop ------------------------------------------------------

    def serve(self, requests):
        while True:
            if self.kc is not None:
                self.pump(POLL_SECONDS)
                timeout = 0
            else:
                timeout = POLL_SECONDS
            while True:
                try:
                    request = requests.get(timeout=timeout) if timeout else requests.get_nowait()
                except queue.Empty:
                    break
                timeout = 0
                if request is None:
                    self.stop_kernel()
                    return
                self.handle(request)
                if self.stopping:
                    return
            now = time.monotonic()
            if self.km is not None and now - self.last_alive_check >= ALIVE_SECONDS:
                self.last_alive_check = now
                self.check_alive()

    def handle(self, request):
        request_id = request.get("id")
        op = request.get("op")
        handler = getattr(self, "op_" + str(op), None)
        if request_id is None or handler is None:
            self.out.event("log", message="invalid bridge request: %s" % json.dumps(request))
            if request_id is not None:
                self.out.fail(request_id, "unknown op %r" % op)
            return
        try:
            handler(request_id, request)
        except Exception as error:
            self.out.fail(request_id, "%s: %s" % (type(error).__name__, error))

    # -- ZeroMQ ---------------------------------------------------------

    def pump(self, timeout):
        sockets = dict(self.poller.poll(int(timeout * 1000)))
        kc = self.kc
        while True:
            busy = False
            for channel, handler in (
                (kc.iopub_channel, self.on_iopub),
                (kc.shell_channel, self.on_shell),
                (kc.control_channel, None),
            ):
                if channel.socket is None:
                    continue
                if sockets.pop(channel.socket, 0) or channel.msg_ready():
                    message = channel.get_msg(timeout=0)
                    busy = True
                    if handler is not None:
                        handler(message)
            if not busy:
                return

    def on_iopub(self, message):
        parent = (message.get("parent_header") or {}).get("msg_id")
        request = self.requests.get(parent)
        msg_type = message["header"]["msg_type"]
        content = message.get("content") or {}
        if msg_type == "status":
            if request is None:
                return
            state = content.get("execution_state")
            if state in ("busy", "idle"):
                self.out.event("status", state=state, request=request)
            if state == "idle":
                execution = self.executions.get(parent)
                if execution is not None:
                    execution.idle = True
                    self.finish(parent)
        elif msg_type == "execute_input":
            if request is not None:
                self.out.event(
                    "execute_input",
                    request=request,
                    execution_count=content.get("execution_count") or 0,
                )
        elif msg_type in FORWARDED:
            self.out.event("output", request=request, msg_type=msg_type, content=content)

    def on_shell(self, message):
        parent = (message.get("parent_header") or {}).get("msg_id")
        content = message.get("content") or {}
        execution = self.executions.get(parent)
        if execution is not None:
            reply = {
                "status": content.get("status", "error"),
                "execution_count": content.get("execution_count"),
            }
            for key in ("ename", "evalue"):
                if key in content:
                    reply[key] = content[key]
            execution.reply = reply
            self.finish(parent)
            return
        request = self.shell_requests.pop(parent, None)
        if request is not None:
            self.out.reply(request, content)

    def finish(self, msg_id):
        execution = self.executions[msg_id]
        if execution.reply is not None and execution.idle:
            del self.executions[msg_id]
            self.out.reply(execution.request, execution.reply)

    def fail_pending(self, error):
        for execution in self.executions.values():
            self.out.fail(execution.request, error)
        self.executions.clear()
        for request in self.shell_requests.values():
            self.out.fail(request, error)
        self.shell_requests.clear()

    def remember(self, msg_id, request):
        self.requests[msg_id] = request
        if len(self.requests) > 2000:
            for key in list(self.requests)[:1000]:
                del self.requests[key]

    # -- kernel lifecycle -----------------------------------------------

    def require_kernel(self):
        if self.kc is None:
            raise RuntimeError("the kernel is not running")

    def check_alive(self):
        if self.kc is None or self.km.is_alive():
            return
        code = None
        process = getattr(self.km.provisioner, "process", None)
        if process is not None:
            code = process.poll()
        message = "The kernel process exited" + (
            " (exit code %s)" % code if code is not None else ""
        )
        self.drop_client()
        self.fail_pending("the kernel died")
        self.out.event("dead", message=message)
        self.out.event("status", state="dead")

    def drop_client(self):
        if self.kc is not None:
            try:
                self.kc.stop_channels()
            except Exception:
                pass
        self.kc = None
        self.poller = None

    def connect(self):
        import zmq

        kc = self.km.client()
        kc.start_channels(hb=False)
        try:
            kc.wait_for_ready(timeout=READY_TIMEOUT)
        except BaseException:
            kc.stop_channels()
            raise
        poller = zmq.Poller()
        for channel in (kc.iopub_channel, kc.shell_channel, kc.control_channel):
            poller.register(channel.socket, zmq.POLLIN)
        self.kc = kc
        self.poller = poller
        info = kc.kernel_info(reply=True, timeout=REQUEST_TIMEOUT)["content"]
        self.language = ((info.get("language_info") or {}).get("name") or "").lower()
        if self.language == "python":
            reply = kc.execute(
                MARIMO_HELPER,
                silent=True,
                store_history=False,
                allow_stdin=False,
                reply=True,
                timeout=REQUEST_TIMEOUT,
            )["content"]
            if reply.get("status") != "ok":
                self.out.event(
                    "log",
                    message="the marimo helper failed: %s: %s"
                    % (reply.get("ename"), reply.get("evalue")),
                )
        # Messages of the startup requests are of no interest.
        for channel in (kc.iopub_channel, kc.shell_channel):
            while channel.msg_ready():
                channel.get_msg(timeout=0)
        return info

    def op_start(self, request, args):
        if self.km is not None:
            raise RuntimeError("the kernel is already started")
        from jupyter_client import KernelManager

        kernel_name = args.get("kernel_name")
        cwd = args.get("cwd") or os.getcwd()
        if kernel_name:
            km = KernelManager(kernel_name=kernel_name)
        else:
            import importlib.util

            if importlib.util.find_spec("ipykernel") is None:
                raise RuntimeError("ipykernel is not installed in %s" % sys.executable)
            from jupyter_client.kernelspec import KernelSpec

            km = KernelManager(kernel_name="python3")
            km._kernel_spec = KernelSpec(
                argv=[sys.executable, "-m", "ipykernel_launcher", "-f", "{connection_file}"],
                display_name="Python 3 (ipykernel)",
                language="python",
                metadata={"debugger": True},
            )
        km.shutdown_wait_time = 1.0
        self.out.event("status", state="starting")
        km.start_kernel(
            cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=None
        )
        self.km = km
        try:
            info = self.connect()
        except BaseException:
            self.stop_kernel()
            self.km = None
            raise
        self.out.event("status", state="idle")
        self.out.reply(request, info)

    def op_restart(self, request, args):
        if self.km is None:
            raise RuntimeError("the kernel is not started")
        self.out.event("status", state="restarting")
        self.fail_pending("the kernel restarted")
        self.drop_client()
        if self.km.has_kernel:
            self.km.restart_kernel(now=True)
        else:
            self.km.start_kernel(**self.km._launch_args)
        info = self.connect()
        self.last_alive_check = time.monotonic()
        self.out.event("status", state="idle")
        self.out.reply(request, info)

    def stop_kernel(self):
        if self.km is None:
            return
        self.drop_client()
        try:
            if self.km.has_kernel:
                self.km.shutdown_kernel(now=False)
        except Exception:
            try:
                self.km.shutdown_kernel(now=True)
            except Exception:
                pass
        try:
            self.km.cleanup_resources()
        except Exception:
            pass

    def op_shutdown(self, request, args):
        self.fail_pending("the kernel shut down")
        self.stop_kernel()
        self.out.reply(request, None)
        self.stopping = True

    def op_interrupt(self, request, args):
        if self.km is None or not self.km.has_kernel:
            raise RuntimeError("the kernel is not running")
        self.km.interrupt_kernel()
        self.out.reply(request, None)

    def op_execute(self, request, args):
        self.require_kernel()
        code = args.get("code") or ""
        marimo = args.get("marimo")
        if marimo is not None:
            code = "__nc_marimo__.run(%r, %r, %r, %r)" % (
                str(marimo.get("cell", "")),
                code,
                [str(name) for name in marimo.get("defs") or []],
                [str(name) for name in marimo.get("refs") or []],
            )
        msg_id = self.kc.execute(code, silent=False, store_history=True, allow_stdin=False)
        self.remember(msg_id, request)
        self.executions[msg_id] = Execution(request)

    def op_complete(self, request, args):
        self.require_kernel()
        msg_id = self.kc.complete(args.get("code", ""), args.get("cursor_pos"))
        self.shell_requests[msg_id] = request

    def op_inspect(self, request, args):
        self.require_kernel()
        msg_id = self.kc.inspect(
            args.get("code", ""), args.get("cursor_pos"), args.get("detail_level", 0)
        )
        self.shell_requests[msg_id] = request

    def op_is_complete(self, request, args):
        self.require_kernel()
        msg_id = self.kc.is_complete(args.get("code", ""))
        self.shell_requests[msg_id] = request

    def op_kernel_specs(self, request, args):
        from jupyter_client.kernelspec import KernelSpecManager

        specs = {}
        for name, entry in KernelSpecManager().get_all_specs().items():
            spec = entry.get("spec") or {}
            specs[name] = {
                "display_name": spec.get("display_name", name),
                "language": spec.get("language", ""),
                "argv": spec.get("argv", []),
            }
        self.out.reply(request, specs)

    def op_invalid(self, request, args):
        raise ValueError(args.get("error", "invalid request"))


def main():
    out = Writer(setup_stdio())
    out.event(
        "hello",
        python=sys.executable,
        version=sys.version.split()[0],
        missing=missing_modules(),
    )
    requests = queue.Queue()
    reader = threading.Thread(target=read_stdin, args=(requests,), name="bridge-stdin")
    reader.daemon = True
    reader.start()
    bridge = Bridge(out)
    try:
        bridge.serve(requests)
    finally:
        bridge.stop_kernel()
    sys.stdout.flush()
    os._exit(0)


main()
