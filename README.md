# keris-ctl

Lightweight settings tool for the **ASUS ROG Keris Wireless** mouse (also Keris EVA Edition and wired Keris), Linux + Windows.
Protocol ported from [G-Helper](https://github.com/seerge/g-helper) (`app/Peripherals/Mouse`).

Settings are written to the mouse's onboard memory, so the app does not need to keep running.

## Features

- 3 onboard profiles
- 4 DPI stages (100 – 16000, step 100)
- Polling rate 125 / 250 / 500 / 1000 Hz
- Button debounce 12 – 32 ms, angle snapping, lift-off distance
- RGB for logo / scroll wheel: static, breathing, color cycle, reactive, battery level, off
- Auto sleep and low-battery warning
- Button remapping (mouse, media and keyboard keys)

## Download

Prebuilt Linux and Windows binaries are on the [Releases](../../releases) page.
To publish a new release, push a tag: `git tag v0.1.1 && git push origin v0.1.1`.

## Build

```sh
cargo build --release
# -> target/release/keris-ctl  (Windows: keris-ctl.exe)
```

Windows: install Rust via rustup (MSVC toolchain). No other dependency is needed.

## Linux permissions

```sh
sudo cp 70-rog-keris.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

## Install (Linux, current user)

```sh
install -Dm755 target/release/keris-ctl ~/.local/bin/keris-ctl
install -Dm644 keris-ctl.desktop ~/.local/share/applications/keris-ctl.desktop
```

## Usage

Run without arguments to open the GUI. CLI:

```
keris-ctl list
keris-ctl info
keris-ctl dpi 1 800
keris-ctl poll 1000
keris-ctl light all static 4 00aaff
keris-ctl sleep 5
keris-ctl bind 4 volup        # see: keris-ctl bind list
keris-ctl --device 2 info     # pick a device from `list`
```
