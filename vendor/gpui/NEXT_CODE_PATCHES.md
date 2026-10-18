# gpui 0.2.2, patched for next_code

A copy of the `gpui` 0.2.2 crate from crates.io (`[patch.crates-io]` in the
workspace `Cargo.toml`), with startup changes to the Windows platform. Every
change is marked `next_code:` in a comment next to it.

- `src/platform/windows/directx_devices.rs`: the Direct3D device is created
  once, on the first adapter that takes it. Upstream created a throwaway
  device per adapter to probe it, then the real one.
- `src/platform/windows/direct_write.rs`: `DirectWritePrepared::load` holds
  everything of the text system that needs no Direct3D device (the factory,
  the system font collection, which is the slow part, the custom font
  collection, the UI font name); `DirectWriteTextSystem::new` takes it plus
  the devices.
- `src/platform/windows/platform.rs`: `WindowsPlatform::new` creates the
  Direct3D device and loads DirectWrite on two threads while the main thread
  initializes OLE, the window class and the message window, then joins
  them. `LazyDropTargetHelper` creates the shell's drag-drop helper on the
  first drag into a window instead of at startup.
- `src/platform/windows/window.rs`: uses the lazy helper.
- `Cargo.toml`: the `image` crate with only the decoders next_code shows
  (bmp, gif, ico, jpeg, png, webp) instead of every format, for a smaller
  executable.

Everything else is unchanged. To update gpui, reapply these on the new
version (`git diff` against the pristine copy of the old one).
