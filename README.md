# dxdwm - BE AWARE OF VIBECODING

`dxdwm` is a small X11 window manager written in Rust.

## What it does

- Becomes the active X11 window manager via `SubstructureRedirect`.
- Manages top-level windows and keeps them focusable/raiseable.
- Default launcher keybind: **Super (Windows key)** runs:
  - `rofi -show drun`
- Terminal keybind: **Alt+Enter** runs:
  - `alacritty`
- Mouse bindings:
  - `Alt + Left Mouse Drag` moves windows.
  - `Alt + Right Mouse Drag` resizes windows.

## Project layout

- `Cargo.toml` - dependency manifest.
- `src/main.rs` - window manager implementation.
- `scripts/run_xephyr.sh` - optional nested X11 runner for Linux.

## Notes

- This is intended for X11 Linux sessions.
- On macOS, this can be edited but not realistically run as an X11 session WM.
- `rofi` must be installed and in `PATH` for the launcher binding.

## License

GPL-3.0-only

## Build (Linux/X11 host)

```bash
cargo build --release
```

## Run as your WM (Linux/X11 host)

```bash
exec /absolute/path/to/dxdwm/target/release/dxdwm
```

## Optional nested run (Linux with Xephyr)

```bash
./scripts/run_xephyr.sh
```



