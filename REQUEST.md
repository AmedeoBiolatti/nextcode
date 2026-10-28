# Ubuntu build dependencies

Run this command in your terminal to install the system dependencies for
building the editor:

```sh
sudo apt-get update && sudo apt-get install -y --no-install-recommends build-essential pkg-config cmake clang libclang-dev libfontconfig-dev libfreetype-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev libxcb1-dev libssl-dev libvulkan-dev libzstd-dev
```

After installing the dependencies and Rust, build and run from the project
folder:

```sh
cargo build --release --workspace --locked -j 2
./scripts/install-linux.sh
./target/release/next_code .
```
