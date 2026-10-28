# Assembly inspection

A [built-in extension](built-in-extensions.md), off until turned on in
**File > Preferences > Manage Extensions…** under **Built-in**. Then open a C, C++ or
Rust source file and choose **Run > Tools > Inspect Assembly**
(also available in the command palette). A separate assembly tab offers
compiler/target selection, **O0–O3** optimization, extra compiler flags,
search, **Copy Assembly**, and the complete compiler command and diagnostics
under **Compiler Output**. **Source** returns to the source editor;
**Refresh** saves open C/C++/Rust sources in the same project folder before
compiling again. Assembly listings are read-only and are not session tabs.

Each **Inspect Assembly** invocation opens another independent tab for the
same file. **New Assembly Tab** in the assembly toolbar does the same.
Numbered tabs keep their own compiler/target, optimization and flags, so
one can stay on O2 while another uses O3 or a different compiler. Drag a tab
to the edge of the editor area to compare the listings side by side.

C/C++ uses a matching `compile_commands.json` entry from the source's
ancestors or their `build` directories, preserving include paths, defines
and language standards. Object output and dependency-generation flags are
removed. Installed Clang/GCC compilers are also offered; use compiler flags
for options such as `-std=c++20`, `-Iinclude` or `-march=native`. Compilation
database entries using response files are skipped. MSVC-style commands
are not supported; select Clang/GCC instead.

Rust uses the containing package's Cargo targets and dependencies, with a
target selector for libraries, binaries, examples and tests. Modules show
the assembly of the selected whole target. O0 uses the development profile;
O1–O3 use release with the selected optimization level. Extra flags go to
`rustc` (for example `-C target-cpu=native`). Files outside Cargo projects
compile as standalone Rust 2024 libraries. Cargo's temporary assembly output
is removed after reading; normal Cargo build artifacts remain in its target
directory. Unreferenced or inlined functions may have no separate assembly.

Compilers run where the project lives, including WSL and dev containers.
Install the relevant toolchain there. Local builds can be cancelled from
the assembly tab; remote builds currently finish or time out before a
cancellation takes effect.
