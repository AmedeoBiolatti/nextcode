# Remote development: WSL and dev containers

## WSL projects

Projects inside a WSL distro are opened by distro and Linux path; the editor
runs on Windows and every project operation (files, watching, search,
language servers, terminal) runs in the distro through `next_code_agent`,
over `wsl.exe` stdin/stdout. Files are never accessed via `\\wsl.localhost`.

Build the Linux agent once (needs Rust inside the distro; it is linked
statically, so it runs in any x86_64 distro) and it is copied next to
`next_code.exe`:

```powershell
scripts\build-wsl-agent.ps1            # or: -Profile debug, -Distro Ubuntu
cargo run --release -p next_code -- --wsl Ubuntu /home/me/project
```

A `\\wsl.localhost\Ubuntu\home\me\project` argument opens the same way, and
`~/project` is expanded in the distro. On first use the agent is installed
to `~/.local/share/next_code/` in the distro (reinstalled when the build
changes) and started through a login shell, so language servers come from the
user's `PATH` there (`rustup component add rust-analyzer`,
`uv tool install basedpyright`). `NEXT_CODE_WSL_AGENT` overrides the agent
binary's location. Each agent that starts points the stable link
`~/.local/share/next_code/next_code_agent` at its build, so CLI agents working
in the distro or container can call `next_code_agent pinpoint …` (see
[docs/pinpoint-for-agents.md](pinpoint-for-agents.md#from-inside-wsl-or-a-dev-container)).

The real-WSL tests are ignored by default:

```powershell
$env:NEXT_CODE_WSL_AGENT = "$PWD\target\release\next_code_agent"
$env:NEXT_CODE_TEST_WSL_DISTRO = "Ubuntu"
cargo test -p remote --test wsl -- --ignored
# notebooks in WSL (needs uv in the distro; the agent next to next_code.exe)
cargo test -p next_code --test headless -- --ignored notebooks_in_wsl
```

The headless notebook tests use `NEXT_CODE_NOTEBOOK_PYTHON` when set; else
they create (once, with `uv`, which needs the network that first time) a
virtualenv with ipykernel and marimo in cargo's `target/tmp/notebook-env`,
else they use a `python3`/`python` that already imports ipykernel; without
any, they print why and pass as skipped.

## Dev containers

A folder with `.devcontainer/devcontainer.json` (or `.devcontainer.json`,
or `.devcontainer/<name>/devcontainer.json`) offers **Reopen in Container**
when opened. The container is brought up with the `docker` CLI where the
folder lives (Docker Desktop on Windows, or docker inside the WSL distro for
WSL folders), then the same agent as for WSL is copied into it and started
with `docker exec`: files, search, git, language servers and terminals all run
in the container, as `remoteUser`, in `workspaceFolder`, with `remoteEnv`.

```powershell
next_code --devcontainer C:\src\project
next_code --devcontainer --wsl Ubuntu /home/me/project
```

`devcontainer:<location>` also works in Open Recent. Supported: `image`,
`build` (Dockerfile, args, target), `dockerComposeFile` + `service` (+
`runServices`), `workspaceFolder`, `workspaceMount`, `mounts` (not for
compose), `containerEnv`, `remoteEnv` (`${containerEnv:PATH}` and the other
`${…}` variables), `remoteUser`, `containerUser`, `runArgs`, `init`,
`privileged`, `capAdd`, `securityOpt`, `overrideCommand`, `shutdownAction`,
and the lifecycle commands (`initializeCommand` on the host; `onCreate`,
`updateContent`, `postCreate` after creation; `postStart` after a start;
`postAttach` each time). Dev Container Features are not installed (a note
names them). The Dev Containers CLI is not needed.

Containers are found by the labels VS Code uses (compose ones by project
and service, named as VS Code names them), so a container VS Code created is
reused. A stopped one is started; one that no longer starts (e.g. a mount
from an old session is gone) is recreated. Containers the editor created or
started are stopped as `shutdownAction` says when its project tab closes.

## Port forwarding (dev containers and WSL)

Like VS Code's Ports view: a server running where the project lives is
reachable on `localhost` here. The **Ports** panel (bottom dock, View >
Ports; Run > Forward a Port…) lists forwarded ports: Port (with its
`portsAttributes` label), Forwarded Address (`localhost:NNNN`, click to open
it in the browser), Running Process, Origin (Auto Forwarded, User Forwarded,
devcontainer.json) and open connections, with Open, Copy (the local
address), Port… (change the local port) and Stop.

- **Forward a Port…** takes `8000` or `host:port` (`db:5432`: the agent
  connects to host `db`, e.g. another compose service). User ports are
  saved per project and forwarded again on reopen.
- **Automatic**: the agent's listening TCP sockets (`/proc/net/tcp{,6}`)
  are polled every 2 s; new ones are forwarded and announced ("Your
  application running on port 8000 is available." with Open in Browser). In
  a container, ports already listening when it opens count too; in WSL only
  ports that start listening afterwards (the distro's services, which WSL
  forwards itself, are left alone). An automatically forwarded port is
  released when its server stops; one the user stopped is not forwarded
  again while it keeps listening. The editor's own debug adapters are
  skipped. Settings: `"ports": { "auto_forward": false }` turns it off.
- **devcontainer.json**: `forwardPorts` (numbers, `"8000"` or
  `"host:port"`) are forwarded on connect, even before anything listens;
  `portsAttributes` (keys: a port, a range `"8000-8010"`, `"host:port"`, or
  a regular expression matched against the process command line) give
  `label`, `protocol` (`http`/`https`, for the browser URL),
  `onAutoForward` (`notify` (default), `openBrowser`, `openBrowserOnce`,
  `openPreview` (opens the browser), `silent`, `ignore`) and
  `requireLocalPort`; `otherPortsAttributes` applies to ports no key
  matches. `elevateIfNeeded` is ignored.
- The local port is the same number on 127.0.0.1 if it is free, else the
  next free one (WSL's own localhost forwarding may hold the same number).
  Each local connection becomes a tunnel through the agent connection
  (protocol 4), so nothing listens inside the container.
- Limits: TCP only (no UDP); local projects have nothing to forward.

## Git credentials and ssh-agent (dev containers and WSL)

Like VS Code's Dev Containers, git inside a dev container (or WSL) project
uses this machine's credentials, so `git push`/`git fetch` there need no
second login:

- **HTTPS**: the agent configures `next_code_agent git-credential` as the
  container user's `credential.helper` (`git config --global`, i.e.
  `~/.gitconfig` there; never the repository's config), only when that
  user has no helper of their own. It asks the editor, which runs
  `git credential fill|approve|reject` here (Windows: `git.exe`, so Git
  Credential Manager, which may show its sign-in window; a Linux/WSL editor:
  its git and helpers) with prompts off and a 3-minute limit (30 s for
  store/erase). Only `http`/`https` credentials are lent; when this machine
  has none, the answer is empty and git in the container prompts as usual.
  Requests are answered only while the project is open; the helper then
  stays in `~/.gitconfig` but answers nothing (git prompts). Credentials
  are never logged.
- **Azure DevOps**: when installing its helper, the agent also sets
  `credential.https://dev.azure.com.useHttpPath=true` if that setting is unset.
  This preserves the organisation path needed by Windows Git Credential
  Manager. Existing Azure-specific settings and user-defined helpers are
  left alone; no credentials are stored in the remote Git configuration.
- **SSH**: this machine's ssh-agent (`SSH_AUTH_SOCK`; on Windows, if unset,
  the OpenSSH agent's `\\.\pipe\openssh-ssh-agent`) is forwarded as a Unix
  socket in the container (`/tmp/next_code-<uid>/host/…-ssh.sock`, or under
  `$XDG_RUNTIME_DIR`), exported as `SSH_AUTH_SOCK` to terminals, tasks,
  language servers and git run through the editor (`ssh-add -l` there lists
  this machine's keys). Pageant is not supported.
- **Identity**: this machine's `user.name`/`user.email` (`git config
  --global`) are copied into the container user's global config when unset
  there.

WSL projects get the same (VS Code also uses Windows' Git Credential
Manager in WSL); a helper the user already set in the distro, e.g. GCM
from Windows, is left alone and keeps working. Settings:
`"devcontainer": { "forward_git_credentials": false }` lends nothing and
removes the helper set earlier on the next open; by hand, in the container:

```sh
git config --global --unset-all credential.helper "^!'.*' git-credential$"
```
