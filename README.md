# Mansher — screenshots, hung out to dry (Linux)

Every screenshot you take hangs on a clothesline at the top of your screen.
Click to copy, hold for options, drag to share, cross to let it go.

## Thanks

Mansher is the Linux clone of [Tendedero](https://github.com/alejandrobujan/tendedero)
(macOS) by Alejandro Bujan. The idea, the interaction language, and the
courage to keep it small all come from his work. Thank you, Alejandro.

> Name note: Tendedero's code is MIT but its name and icon are its own.
> `Mansher` (منشر — clothesline) plus an original icon keep this port
> respectful and clean for sharing.

## Features

- Clothesline overlay for screenshots (layer-shell on Niri/Sway/Hyprland/KDE,
  fallback window elsewhere, including GNOME and X11)
- Click a shot to copy it, right-click for keep/trash/share actions
- Drag shots out as real files, cross to discard
- Watches your screenshots folder, desktop, and clipboard — no capture tool required
- System tray menu plus a Preferences window (also at `mansher-app --preferences`)
- Sounds plus launch-at-login settings
- Private by design: no account, no network, no analytics

## Build

```sh
# Core only — works anywhere:
cargo test -p mansher-core

# Full UI needs GTK4 dev headers:
# Ubuntu/Debian: sudo apt install libgtk-4-dev libadwaita-1-dev
# Fedora:        sudo dnf install gtk4-devel libadwaita-devel
# Arch:          sudo pacman -S gtk4 libadwaita
cargo build --release -p mansher-app
```

## Install and run

Grab `Mansher-x86_64.AppImage` from the
[releases](https://github.com/TAGOOZ/Mansher/releases) page, make it
executable, and run it. Or run the built binary detached:

```sh
setsid ./target/release/mansher-app >/tmp/mansher.log 2>&1 < /dev/null & disown
timeout 8 ./target/release/mansher-app --toggle
```

Take a screenshot with your usual tool (grim + slurp, GNOME Screenshot,
Spectacle, Flameshot). It lands on the line. `mansher-app --doctor` reports
what the app can see on your machine when something looks wrong.

## Distribute

- **AppImage**: one file, runs unsandboxed with full host access.
- **Flatpak** (`flatpak/`, Flathub-bound): one build, portal permissions.
- **Native**: `.deb` / `.rpm` / AUR for layer-shell hover plus grim/slurp setups.
- `Cargo.lock` is committed so every build resolves identical dependencies.

## License

MIT.
