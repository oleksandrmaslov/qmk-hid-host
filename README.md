# QMK HID Host

Host component for communicating with QMK keyboards using Raw HID feature.

Requires support on keyboard side, currently is supported by [stront](https://github.com/zzeneg/stront).

## Architecture

Application is written in Rust which gives easy access to HID libraries, low-level Windows/Linux APIs and cross-platform compatibility.

## Supported platforms/providers

|              | Windows            | Linux                           | MacOS                        |
| ------------ | ------------------ | ------------------------------- | ------------------           |
| Time         | :heavy_check_mark: | :heavy_check_mark:              | :heavy_check_mark:           |
| Volume       | :heavy_check_mark: | :heavy_check_mark: (PulseAudio) | :heavy_check_mark:           |
| Input layout | :heavy_check_mark: | :heavy_check_mark: (X11, KDE Wayland) | :heavy_check_mark:           |
| Media info   | :heavy_check_mark: | :heavy_check_mark: (MPRIS/D-Bus, Chromecast)[^linux-spotify] | :heavy_check_mark: (Spotify) |
| Relay        | :heavy_check_mark: | :heavy_check_mark:              | :heavy_check_mark:           |
| Weather      |  | :heavy_check_mark: | :heavy_check_mark: |

MacOS is partially supported, as I don't own any Apple devices, feel free to raise PRs.

On Linux, KDE browser integration replaces Chrome's native tab-title source when
its metadata identifies the same browser process. Other media sources keep their
normal playing/paused priority; equal states prefer a source with an artist tag.
Missing artist tags clear the previous artist, and media workers stop between
reconnections so an older source cannot keep overwriting the display.
Missing titles clear the previous title and compact display; a browser name such
as "Chrome" is never used as a track title. These fields also synchronize on
connection when empty. An optional Chromecast source reads track details directly
from the receiver, including when the browser stops publishing MPRIS metadata.

## Relay mode (device-to-device communication) - experimental

This allows for communication between two or more devices. `qmk-hid-host` only receives information from any device and broadcasts it to all devices. The actual sending and receiving should be configured in devices' firmware, but you have to set first byte in the data array - `0xCC` for sending and `0xCD` for receiving.

Example for syncing layers between two devices:

### Data type enum (common between `qmk-hid-host` and all devices)

```c
typedef enum {
    _TIME = 0xAA, // random value that does not conflict with VIA, must match companion app
    _VOLUME,
    _LAYOUT,
    _MEDIA_ARTIST = 0xAD,
    _MEDIA_TITLE = 0xAE,
    _WEATHER = 0xAF,
    _MEDIA_PLAYER_LINUX = 0xB0,
    _MEDIA_EXTENDED = 0xB1,

    _RELAY_FROM_DEVICE = 0xCC,
    _RELAY_TO_DEVICE,
} hid_data_type;
```

### Source device

```c
typedef enum {
    _LAYER = 0,
} relay_data_type;

layer_state_t layer_state_set_user(layer_state_t state) {
    uint8_t data[32];
    memset(data, 0, 32);
    data[0] = _RELAY_FROM_DEVICE;
    data[1] = _LAYER;
    data[2] = get_highest_layer(state);
    raw_hid_send(data, 32);

    return state;
}
```

#### Destination device

```c
typedef enum {
    _LAYER = 0,
} relay_data_type;

void raw_hid_receive_kb(uint8_t *data, uint8_t length) {
    if (data[0] == _RELAY_TO_DEVICE) {
        switch (data[1]) {
            case _LAYER:
                layer_move(data[2]);
                break;
        }
    }
}
```

## How to run it

All files are available in [latest release](https://github.com/zzeneg/qmk-hid-host/releases/tag/latest).

### Configuration

Default configuration is set to [stront](https://github.com/zzeneg/stront). For other keyboards you need to modify the configuration file (`qmk-hid-host.json`).

- `devices` section contains a list of keyboards
  - `productId` - `pid` from your keyboard's `info.json`. You can get it by running `qmk-hid-host -p`
  - `name` - keyboard's name (optional, visible only in logs)
  - `usage` and `usagePage` - optional, override only if `RAW_USAGE_ID` and `RAW_USAGE_PAGE` were redefined in firmware
- `layouts` - list of supported keyboard layouts in two-letter format (app sends layout's index, not name)
- `reconnectDelay` - delay between reconnecting attempts in milliseconds (optional, default is 5000)
- `weather` - optional weather provider config for Linux and MacOS. The URL should return a temperature value, for example `wttr.in/Hamburg?format=%t`
- `chromecast` - optional Linux Cast receiver hostname or IP, for example `"chromecast": "living-room.local"`.

Use a stable `.local` hostname or a reserved IP for your Cast receiver on the same
network. The host reads its music track or generic media title on TCP port 8009
once per second. Playing or buffering Cast media takes priority; paused Cast
media yields to a playing local app. Idle, unreachable, or unnamed Cast media
falls back to MPRIS. It observes an existing session without changing playback,
and needs no keyboard firmware change. Omit `chromecast` to disable it.
Cast devices use self-signed TLS certificates, so configure a receiver on a
trusted local network.
The receiver can be playing a session started by another phone or computer;
the host only needs network access to the configured receiver.
The Cast reader is independent of the Linux distro and desktop environment.
For `.local` hostnames, the system resolver must support mDNS. On Arch-based
systems, follow the [Avahi hostname resolution setup](https://wiki.archlinux.org/title/Avahi#Hostname_resolution)
(`avahi`, `nss-mdns`, the service, and NSS configuration), or configure a reserved
IP instead. Check hostname resolution with `getent hosts living-room.local`.
The read-only hardware check requires an active track:
`QMK_CHROMECAST_HOST=living-room.local cargo test live_chromecast_reports_track_and_stops_monitor -- --ignored --nocapture`.

On KDE Wayland, layout updates use `org.kde.keyboard` on the session D-Bus.
The active layout is sent on connection, then updated on KDE layout-change
notifications. Other Wayland compositors are not supported by this provider.
X11 sessions continue to use XKB.

Keep each layout as a separate entry, in the same order as the keyboard's
display labels. For Wafer's `US,RU,DE,UA` labels, use
`"layouts": ["us", "ru", "de", "ua"]`, even if KDE lists them in a different
order. KDE layout names are mapped to these indices before sending.

The live KDE regression check briefly cycles the layouts and restores the active
one: `cargo test kde_layout_reports_match_firmware_order -- --ignored`.
It expects KDE layouts `us`, `ru`, `de`, and `ua`, in any system order.

#### Minimal config

```json
{
  "devices": [
    {
      "productId": "0x0844"
    }
  ],
  "layouts": ["en"],
  "weather": {
    "url": "wttr.in/Hamburg?format=%t"
  }
}
```

Configuration is read from file `qmk-hid-host.json` in the current working directory. If it is not found, then the default configuration is written to this file.
You can specify a different location for the configuration file by using `--config (-c)` command line option. For example:

```
qmk-hid-host -c $HOME/.config/qmk-hid-host/config.json
```

### Windows

#### Manual/Debug mode

1. Start `qmk-hid-host.exe`
2. If needed, edit config and restart the app

#### Silent mode

When you verified that the application works with your keyboard, you can use `qmk-hid-host.silent.exe` instead (like add it to Startup). It does not have a console or logs, and can be killed only from Task Manager.

### Linux

1. Update `udev` rules by running script (remember to update `idVendor` and `idProduct` to your values first):

   ```sh
   sudo sh -c 'echo "KERNEL==\"hidraw*\", SUBSYSTEM==\"hidraw\", ATTRS{idVendor}==\"feed\", ATTRS{idProduct}==\"0844\", MODE=\"0666\"" > /etc/udev/rules.d/99-qmkhidhost.rules'
   ```

   [More info](https://get.vial.today/manual/linux-udev.html)

2. Install runtime dependencies for the enabled providers:

   ```sh
   sudo pacman -S curl
   ```

   Local Linux media info uses D-Bus/MPRIS, so the active player must expose MPRIS metadata. Spotify works out of the box. Other players, like mpv, depend on how they publish metadata. The optional `chromecast` source reads metadata directly from a Cast receiver.

   The Linux media provider sends `_MEDIA_ARTIST` (0xAD) on artist changes, and `_MEDIA_TITLE` (0xAE) and `_MEDIA_PLAYER_LINUX` (0xB0, 8 bytes space-padded compact text) on title changes. When `"extendedMedia": true` is set in config, it also sends `_MEDIA_EXTENDED` (0xB1) on local play/pause/track-change events and once per second for Cast media, containing: 2-byte total time (seconds, u16 LE), 2-byte current position (seconds, u16 LE), 1-byte playback status (0=stopped, 1=playing, 2=paused), 1-byte artist name length, and the artist name (up to 21 bytes).

   Weather uses `curl` and the configured `weather.url`; the default expects a wttr.in response like `+29°C`.

3. Reconnect keyboard
4. Start `qmk-hid-host`, add it to autorun if needed

### MacOS
> [!NOTE]
> To configure the weather, you need to replace your `Hamburg` with your city (example: Cairo); here is its respective repository to see more configurations: [chubin/wttr.in](https://github.com/chubin/wttr.in) - ⛅ The right way to check the weather

1. Download `qmk-hid-host`
2. Modify `qmk-hid-host.json`
3. Add your layouts or your local weather, for example:

   ```json
   "layouts": ["ABC", "Russian"],
   "weather": {
     "url": "wttr.in/Hamburg?format=%t"
   ```

   if you don't know what layout are installed in you system, run qmk-hid-host with the layouts listed above, change lang and look at terminal output:

   ```
   INFO qmk_hid_host::providers::layout::macos: new layout: 'ABC', layout list: ["ABC", "Russian"]
   INFO qmk_hid_host::providers::layout::macos: new layout: 'Russian', layout list: ["ABC", "Russian"]
   ```

   "new layout:" is what you need

4. start `qmk-hid-host` from directory where your `qmk-hid-host.json` is located

   Note: macOS, by default, may not locate your configuration file correctly. It's recommended to start `qmk-hid-host` with the configuration file path explicitly specified, for example:
   `./qmk-hid-host -c ~/Downloads/macos/qmk-hid-host.json`

5. If you `qmk-hid-host` stuck at `Waiting for keyboard...` there are two common mistakes:
   1. You're wrong with productId in your config. Check `qmk-hid-host -p`
   2. Close Vial app and try again

## Development

### Nix

1. `nix develop`

### Native

On Arch-based systems, install the build dependencies:

```sh
sudo pacman -S --needed base-devel rust dbus libx11 libpulse systemd-libs
```

If Rust is managed through rustup, use its current stable toolchain instead of
installing the `rust` package. The existing `install-build-deps.sh` uses APT for
Debian/Ubuntu; other distros need their equivalent development packages for
libudev, D-Bus, X11, and libpulse. The Cast client is built into the executable
and does not require a separate Cast utility or OpenSSL installation. For an
older distro or different CPU architecture, build locally with its toolchain
and libraries instead of copying an incompatible release binary.

1. Install Rust
2. Run `cargo run`
3. If needed, edit `qmk-hid-host.json` in root folder and run again

## Changelog

- 2026-06-19 - add Linux weather, compact media player, and extended media HID support
- 2025-11-11 - add support for weather and spotify with MacOS
- 2024-10-03 - add support for multiple devices, restructure config
- 2024-09-15 - add MacOS support
- 2024-02-06 - add Linux support
- 2024-01-21 - remove run as windows service, add silent version instead
- 2024-01-02 - support RUST_LOG, run as windows service
- 2023-07-30 - rewritten to Rust

[^linux-spotify]: Tested with Spotify on Linux, where media metadata is reported correctly.
