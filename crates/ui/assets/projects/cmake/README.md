# @PROJECT_NAME@

A @LANGUAGE_NAME@ application built with CMake 3.20 or newer and a @LANGUAGE_NAME@ compiler.

In NextCode, select **Build and Run** in the toolbar and click Run. The
configuration first configures CMake, then builds and runs `build/bin/app`
(`app.exe` on Windows). **Build** configures and builds without running.
After building, select **Debug (build first)** and click Debug to use
NextCode's CodeLLDB integration; install the adapter when prompted if needed.

From a terminal in this folder:

```sh
cmake -DRUN_APP=ON -P cmake/Run.cmake
```

The default build includes debug information and uses @LANGUAGE_NAME@ @STANDARD@.
CMake exports `build/compile_commands.json` with Makefile and Ninja generators.
The `.clangd` file tells clangd to read this compilation database for completion
and diagnostics. Install clangd if it is not already available.
