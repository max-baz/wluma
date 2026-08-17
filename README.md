# wluma

A tool for Wayland compositors to automatically adjust screen brightness based on the screen contents and amount of ambient light around you.

## Supported screen capture protocols

With the default `capturer = "auto"`, `wluma` tries supported Wayland protocols followed by PipeWire sources. If none works during selection, it uses ambient light only and periodically retries. Once a protocol produces usable frames, `wluma` keeps using that protocol and reconnects it after interruptions. See the "Configuration" section below for details.

The list of supported protocols:

- `ext-image-copy-capture-v1` - the newest protocol that potentially is (or will be) supported by any modern Wayland desktop environment.
  - requires `ext-image-capture-source-v1` and `linux-dmabuf-v1` protocols to be supported as well.
- `wlr-screencopy-unstable-v1` - supported by any `wlroots`-based compositors (e.g. `sway`), as well as Hyprland.
  - requires `linux-dmabuf-v1` protocol to be supported as well.
- `wlr-export-dmabuf-unstable-v1` - supported by any `wlroots`-based compositors (e.g. `sway`).
- PipeWire streams provided by:
  - KWin's private screencast protocol.
  - Mutter's private ScreenCast API.
  - Generic ScreenCast portal.

## Idea

The app will automatically brighten the screen when you are looking at a dark window (such as a fullscreen terminal) and darken the screen when you are looking at a bright window (such as web browser). The algorithm takes into consideration the amount of ambient light around you, so the same window can be brighter during the day than during the night.

With permission of [Lumen](https://github.com/anishathalye/lumen)'s author (the project that inspired me to create this app), I'm reusing a demo GIF:

![demo](https://user-images.githubusercontent.com/1177900/82347130-8bd22b80-99f7-11ea-8545-0d311240a30d.gif)

## Usage

Simply launch `wluma` and continue adjusting your screen brightness as you usually do - the app will learn your preferences.

`wluma` will not do anything on the first launch! You have to adjust the brightness by hand a few times, in different environment and/or with different screen contents, that way `wluma` will learn your preferences and only then it will begin to automatically change your screen brightness for you.

Changing brightness, seeing status and controlling the running daemon can be also done through CLI commands, run `wluma help` to see all available options.

## Performance

The app has minimal impact on system resources and battery life even though it is able to monitor screen contents several times a second. This is achieved by importing DMA-BUF screen buffers and doing computations on GPU using Vulkan API.

## Installation

<a href="https://repology.org/project/wluma/versions">
  <img src="https://repology.org/badge/vertical-allrepos/wluma.svg" alt="Packaging status" align="right">
</a>

Use one of the available packages and methods below:

- Alpine Linux: [wluma](https://pkgs.alpinelinux.org/packages?name=wluma)
- Arch Linux: [wluma](https://aur.archlinux.org/packages/wluma/) or [wluma-git](https://aur.archlinux.org/packages/wluma-git/)
- NixOS: [wluma](https://search.nixos.org/packages?channel=unstable&show=wluma&from=0&size=50&sort=relevance&type=packages&query=wluma)
- Fedora Linux: [wluma](https://github.com/terrapkg/packages/tree/frawhide/anda/system/wluma) via the [Terra repository](https://terra.fyralabs.com/)
- Build the app yourself using the instructions below and copy the resulting binary somewhere in your `$PATH`.
  - optionally, grab the `wluma.service` if you want to run it as a systemd-service - it can be placed e.g. in `~/.config/systemd/user/`.
  - you might need `90-wluma-backlight.rules` too, if you want to give `wluma` direct driver access for the fastest performance (see "Permissions" section below) - it can be placed e.g. in `/etc/udev/rules.d/`.

## Build

[![CI](https://github.com/maximbaz/wluma/actions/workflows/ci.yml/badge.svg)](https://github.com/maximbaz/wluma/actions/workflows/ci.yml)

If you want to build the app yourself, make sure you use latest stable Rust, otherwise you might get compilation errors! Using `rustup` is perhaps the easiest. Ubuntu needs the following dependencies: `sudo apt-get -y install v4l-utils libv4l-dev libudev-dev libvulkan-dev libdbus-1-dev libpipewire-0.3-dev`.

Then simply run `cargo build --locked --release` and the binary will be placed into `./target/release/wluma`.

## Permissions

In order to access backlight devices, `wluma` must either:

- have direct driver access: install the supplied `90-wluma-backlight.rules` udev rule, add your user to the `video` group and reboot (fastest, most likely a requirement for smooth transitions)
- run on a system that uses `elogind` or `systemd-logind` (they provide a safe interface for unprivileged users to control device's brightness through `dbus`, no configuration necessary)
- run as `root` (not recommended)

## Configuration

The configuration is entirely optional, as everything is auto-detected to the best extent possible. To make any changes described below, create `$XDG_CONFIG_HOME/wluma/config.toml` and set the relevant overrides.

### ALS

When ALS configuration is omitted, wluma uses an external ALS whenever `$XDG_RUNTIME_DIR/wluma/als.sock` is a Unix socket, otherwise it uses an available IIO ambient light sensor or continues without one. This selection is updated while wluma is running as sources appear and disappear. For IIO it first tries `iio-sensor-proxy` over the system D-Bus and then direct IIO discovery under `/sys/bus/iio/devices`.

Explicit `[als.external]`, `[als.iio]`, `[als.webcam]`, `[als.time]` and `[als.none]` sections override automatic selection. The IIO `path` enables direct polling from a different sysfs directory when `iio-sensor-proxy` is unavailable.

```toml
[als.iio]
path = "/sys/bus/iio/devices"
```

An external ALS is a Unix stream socket server that sends one non-negative decimal value per line. It should send values as soon as they become available and at least once every two seconds, including when unchanged. The path defaults to `$XDG_RUNTIME_DIR/wluma/als.sock` and the scale defaults to `lux`:

```toml
[als.external]
path = "/run/user/1000/home-assistant/als.sock"
scale = "linear"
```

The `lux` scale accepts illuminance values without an upper bound. The `linear` scale accepts normalized values from 0 to 100. The socket is detected whether it exists before wluma starts or appears later.

Webcam ALS reports perceived camera-frame lightness from 0 to 100. Set `video` to the zero-based V4L2 device number, such as `0` for `/dev/video0`:

```toml
[als.webcam]
video = 0
```

Time ALS uses a circular, linearly interpolated schedule of synthetic light levels. Hours must be from 0 to 23 and levels from 0 to 100:

```toml
[als.time]
levels = { 0 = 0, 7 = 10, 9 = 40, 12 = 70, 16 = 50, 19 = 10, 21 = 0 }
```

To explicitly disable ambient-light input, use:

```toml
[als.none]
```

The adaptive predictor remembers your adjustments and applies them across the full range of ambient light and screen content, including conditions you have not encountered before. It reproduces learned preferences when possible and balances conflicting preferences instead of discarding them.

### Idle dimming

After two minutes of inactivity, wluma pauses automatic adjustment, dims displays to 30% of their current brightness and turns keyboard backlights off. These defaults can be changed globally or per power source:

```toml
[idle]
enabled = true
timeout = 120
brightness = 30

[idle.ac]
timeout = 300

[idle.battery]
brightness = 20
```

Timeouts are in seconds. Omitted AC and battery settings inherit the global values; set `enabled = false` globally or in either profile to disable it. Wluma uses UPower to detect the current power source. When activity resumes, automatic adjustment continues with a fresh prediction rather than explicitly restoring the previous brightness.

Idle detection uses `ext-idle-notify-v1`, which honors compositor idle inhibitors, with GNOME Mutter's IdleMonitor as a fallback. Disable other automatic idle dimming to avoid conflicts. On GNOME:

```sh
gsettings set org.gnome.settings-daemon.plugins.power idle-dim false
```

### Displays

The entire `output` section is optional. Wluma continuously discovers connected DRM outputs, associates internal panels with `/sys/class/backlight` devices and external monitors with DDC using their EDID, and automatically selects a screen capturer. Automatically discovered outputs and keyboards are started and stopped as they appear and disappear. DDC is known to often be problematic, so always consider trying [ddcci-driver-linux](https://gitlab.com/ddcci-driver-linux/ddcci-driver-linux) first if you can.

To override the capturer, predictor or gamma control for a discovered output, use the detected brightness type and DRM connector name reported with `RUST_LOG=debug`:

```toml
[[output.ddcutil]]
name = "HDMI-A-3"
capturer = "none"
```

Explicit `output.backlight` and `output.ddcutil` entries remain supported. The `path` of a backlight entry is optional when its `name` matches a discovered connector. When set, it must point to a sysfs backlight device directory such as `/sys/class/backlight/intel_backlight`. Supplying a path or DDC identifier overrides the discovered value. `enabled`, `capturer`, `vulkan_device`, `predictor` and `gamma` are available for both output types. To exclude a discovered output, set `enabled = false`:

```toml
[[output.ddcutil]]
name = "DP-1"
enabled = false
```

The `name` field identifies the Wayland output and is matched as a substring against descriptions containing model, manufacturer and serial number (like `eDP-1 'Sharp Corporation 0x14A8 0x00000000' (eDP-1)`). Automatically discovered output names use the unambiguous DRM connector name.

For `output.ddcutil`, if the identifier that works for brightness control is not the same substring that your Wayland compositor exposes for screen capture, set `identifier`. This is common for external DDC monitors: `name` should match the compositor output description such as `HDMI-A-3`, while `identifier` may need to be a serial number for DDC.

_Tip:_ run `wluma` with `RUST_LOG=debug` to see how your outputs are being identified, so that you can choose an appropriate `name` and `identifier` configuration values.

The `capturer` field will determine how screen contents will be captured. Currently supported values are `auto`, `wayland`, `pipewire` and `none` (ignores screen contents and predicts brightness only based on ALS). The default value `auto` tries supported Wayland protocols first, then PipeWire sources. If none works during selection, `wluma` uses ALS alone and periodically retries. Once a protocol successfully produces frames, it remains selected and is reconnected after interruptions; brightness prediction pauses until capture recovers rather than switching protocols or assuming zero screen luma. The value `wayland` chooses among Wayland protocols in the same way, but you can force a specific one with `ext-image-copy-capture-v1`, `wlr-screencopy-unstable-v1` or `wlr-export-dmabuf-unstable-v1`. The value `pipewire` similarly chooses among PipeWire sources, and you can force a specific source with `zkde-screencast-unstable-v1`, `gnome-mutter-screencast` or `xdg-desktop-portal-screencast`.

_Tip:_ run `wluma` with `RUST_LOG=debug` and `capturer="auto"` to see which protocols are supported and which capturer `wluma` chooses.

On multi-GPU systems, wluma automatically selects a Vulkan device. Set `vulkan_device` only if it selects the wrong GPU:

```toml
[[output.backlight]]
name = "eDP-1"
vulkan_device = "/dev/dri/renderD128"
```

Run `wluma` with `RUST_LOG=debug` and find the `Discovered Vulkan device` entry for the GPU you want to use. Set `vulkan_device` to its `DRM render device` path, such as `/dev/dri/renderD128`; do not use the `DRM primary device` path.

When the ScreenCast portal is used, select the monitor matching the configured output on the first run. wluma asks the portal to persist this selection and stores its restore token in the XDG state directory, so supported portal backends can restore it without prompting after restart. A separate portal session and restore token are used for each configured output.

### Dimming and color temperature

On supported wlroots compositors, Hyprland and GNOME, wluma can also learn and adjust per-output dimming and color temperature. Dimming ranges from 0% to 100%, and color temperature from 1000K to 25000K (6500K is neutral). Dimming is learned from both ambient light and screen luma; color temperature is learned from ambient light only, avoiding color shifts as screen content changes.

```sh
wluma set dim DP-1 20%
wluma set dim DP-1 +5%
wluma set temperature DP-1 4500K
wluma set temperature DP-1 +500K
wluma set temperature DP-1 neutral
```

Wayland gamma control is exclusive, so wluma cannot share an output with Gammastep, wlsunset or another gamma-control client. Wluma continues without these features when gamma control is unavailable. To prevent wluma from acquiring gamma control for an output, set `gamma = false` in its `output.backlight` or `output.ddcutil` entry:

```toml
[[output.backlight]]
name = "eDP-1"
gamma = false
```

### Keyboards

Keyboard backlights are automatically discovered under `/sys/class/leds` when their LED device name contains `kbd_backlight`, which covers common Dell, ThinkPad and ASUS devices. They are logged with `RUST_LOG=debug`. Explicit `[[keyboard]]` entries remain available for devices that do not follow this naming convention, and an entry pointing to an automatically discovered path replaces rather than duplicates it. `name` must be unique across all displays and keyboards, while `path` must point to the keyboard LED's sysfs directory:

```toml
[[keyboard]]
name = "keyboard"
path = "/sys/class/leds/asus::kbd_backlight"
```

### Algorithm

The default algorithm that `wluma` uses is called `adaptive`, which is when it learns from you as you continue adjusting brightness manually. It will eventually figure out patterns in how you tend to adjust brightness in dark and lit conditions and depending on what is currently being displayed on the screen, and will beging to do it automatically for you.

If you instead want to preserve control over absolute brightness value, but let `wluma` only do relative adjustments, there is an alternative algorithm called `manual`. It can be useful if you feel like `wluma` is unable to learn the patterns, for example because you don't have a real ambient light sensor, and neither of the alternative ALS inputs are able to capture the real light conditions precisely enough.

The manual predictor accepts points containing an ALS value, screen luma and brightness reduction percentage:

```toml
[[output.backlight]]
name = "eDP-1"

[output.backlight.predictor.manual]
[[output.backlight.predictor.manual.points]]
als = 0
luma = 0
reduction = 0

[[output.backlight.predictor.manual.points]]
als = 0
luma = 100
reduction = 60

[[output.backlight.predictor.manual.points]]
als = 500
luma = 0
reduction = 0

[[output.backlight.predictor.manual.points]]
als = 500
luma = 100
reduction = 10
```

`luma` is the whiteness of the screen contents from 0 to 100. `reduction` is the percentage by which the current manually selected brightness is reduced. `als` uses lux for IIO, perceived lightness for webcam, configured synthetic level for time, and 0 for none. The algorithm continuously interpolates between nearby points.

## Run

To run the app, simply launch `wluma` or use the provided systemd user service.

## Debugging

To enable logging, set environment variable `RUST_LOG` to one of these values: `error`, `warn`, `info`, `debug`, `trace`.

For more complex selectors, see [env_logger's documentation](https://docs.rs/env_logger/latest/env_logger/#enabling-logging).

## Validating that wluma is able to see screen contents correctly

This is a useful test to validate that wluma does indeed see the screen contents correctly. This is obviously only applicable if you didn't disable `capturer` in your config.

1. Stop any running `wluma` instances
1. Run the latest code from `main` branch (unless another branch was given to you by the maintainers): `RUST_LOG=trace cargo run`
1. Open https://deadpixeltest.org/ and start the test.
1. Make sure that **the entire screen** is covered with a single solid color, nothing else should be visible - not a status bar nor a notification, nothing else.
1. Repeat for each of these colors: `black`, `white`, `red`, `green`, `blue`:
   1. Let the color be visible for a few seconds.
   1. Quickly go back to the running `wluma` and check the `luma` value reported for that color: `Prediction: 252 (als: 0, luma: ---> 14 <---)`
1. Compare your values with the following expected results:
   ```
   black: 0
   white: 100
   red: 49
   green: 83
   blue: 26
   ```

If your results do not match, please open an issue and let's investigate!

## Relevant projects

- [lumen](https://github.com/anishathalye/lumen): project that inspired me to create this app
