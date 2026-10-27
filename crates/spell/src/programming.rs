//! Words common in code and its comments that dictionaries lack (like
//! cSpell's software-terms list). Lowercase; parts of identifiers count
//! too (`stdout`, `kwargs`, `readonly`).

use std::{collections::HashSet, sync::LazyLock};

static WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| LIST.split_whitespace().collect());

/// Whether `word` (lowercase) is a known programming word.
pub(crate) fn contains(word: &str) -> bool {
    WORDS.contains(word)
}

const LIST: &str = "
abc abi accessor accessors addr aes ajax alloc allocator allocs amd ansi api apis
arg argc args argv arity arr asc ascii asm assert asserts async atexit atoi atol
auth authn authz autocomplete autoformat autoload autoreload autosave aws axios
backend backends backoff backport backtick backticks backtrace base64 basename
bitfield bitflags bitmask bitset bitwise blockquote bool boolean booleans bootstrap
borrowck boto btree buf bufs bufio builtin builtins bytearray bytecode bytestring
callee callees callsite callback callbacks cfg cgroup changelog charset chdir chmod
chown chrono chunked cli clippy cmake cmd cmds cmp codebase codegen codepoint
codepoints colormap commonjs comparator composable concat config configs const
consts contextvar contextvars coroutine coroutines cpp cpu cron crlf css csv ctor
ctx ctypes cuda cwd daemon dataclass dataclasses dataframe dataframes dataset
datasets datetime dbg debounce debounced debouncing debugpy dedent dedup dedupe
deduplicate deduped deps deque deref derefs deserialization deserialize
deserialized deserializer deserializes desc dest destructure destructured
destructuring dev devcontainer devtools dict dicts dir dirname dirs django dll
dlls dns docker dockerfile docstring docstrings dotenv dropdown dst dtype dtypes
dyn ecmascript elif emoji emojis enqueue enqueued enum enums env envs eof eol
epoch errno esm eslint eval evals exe expr exprs fastapi fdopen fetchall fifo
filename filenames filepath filesystem filesystems filetype fixme flexbox fmt
fn fns formatter formatters fstring fsync func funcs gcc getattr getenv getter
getters github gitignore gitlab glibc globals glob globs golang goroutine
goroutines gpu gpui grpc gui gzip hashable hashmap hashmaps hashset hasher
hashers hostname href html http https hunspell iife impl impls importlib init
inits inlined inlining inlay inode inodes int ints io ioctl iter iterable
iterables iters itertools javascript jinja jit jpeg json jsonl jsx jwt kotlin
kubectl kubernetes kwarg kwargs lambda lambdas len lexer lib libc libs lifecycle
linter linters linux localhost lockfile lookahead lookbehind lru lsp lstrip
macos malloc matplotlib memcpy memoize memoized memset metadata middleware
middlewares minify minified mkdir mmap monorepo mpsc msg msgs musl mut mutex
mutexes mypy namespace namespaced namespaces nan nginx nodejs noop noqa npm
nullable nullptr num numpy nums oauth objc onclick onload ok openapi param
params parsers pathlib pathname pid pids pipenv png polyfill polyfills popen
posix postgres postgresql pragma prepend prepended prepends printf println
proc procs protobuf proxied pseudocode ptr ptrs pub pydantic pyi pylint
pyproject pytest python pythonic qualname quickfix rails readline readme
readonly realloc realtime recurse redis redux refactor refactored refactoring
refcount regex regexes regexp repl repo repos req reqs rerender rerun rerunning
resize resized resizing rmdir rng rpc rstrip rtl runtime runtimes rustc rustfmt
rustup sbin scipy sdk sdks semver serde serializable sha shebang sigint sigterm
sizeof sklearn sql sqlite src ssh ssl stacktrace stateful stateless stderr
stdin stdio stdlib stdout str strerror strftime stringify strlen strptime strs
struct structs subclass subclassed subclasses subclassing subcommand subcommands
subdir subdirs subdirectory subdirectories submodule submodules subprocess
subprocesses substr subtree subtrees sudo svg symlink symlinked symlinks
syntaxes sys syscall syscalls systemd tcp tempdir tempfile templating
textarea timedelta timestamp timestamps tls tmp todo todos toml tooltip
tooltips toolchain tsconfig tsx tty tuple tuples typealias typecheck typechecker
typedef typedefs typeof typescript typing ubuntu udp uid uint ulimit unary
uncomment uncommented unescape unicode unindent uninstall unix unlink unmount
unpickle unregister unset unshift unsubscribe untracked unwrap unwraps
upsert uri uris url urls usb usize utf utils uuid validator validators varargs
vec vecs venv verbose virtualenv vite vscode wasm webhook webhooks webpack
websocket websockets whitespace wildcard wildcards workflow workflows
workspace workspaces wrapper wrappers wsl xml xpath yaml yml zlib
";
