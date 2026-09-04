# rkvm

> **This is a maintained fork of [htrefil/rkvm](https://github.com/htrefil/rkvm).**
> Upstream has been quiet for a while, so this fork carries bug fixes and new features.
> Issues and pull requests are welcome.

rkvm is a tool for sharing a keyboard and mouse across multiple Linux machines.

One machine runs the server. It owns the physical keyboard and mouse and relays what you do
to the clients. A key combination decides which machine currently receives your input.

## Features

**Switching**
Press the switch keys and control moves to the next machine, then wraps back around to the
server. Every machine sees the same keycodes you actually pressed, so your keyboard layout
never enters the picture.

**Named machines**
Give a client a `name` and the server can jump straight to it with its own shortcut instead
of cycling through everything. The name `server` is reserved for the machine holding the
keyboard, so you always have a way back.

**Clipboard sharing**
What you copy on one machine becomes available on all of them, text and images alike. rkvm
does not talk to your display server. You give it three commands (`wl-copy`, `xclip`, or
anything else), it runs them once a second, and sends what changed to the other machines.

**Switch hooks**
The server can run a command every time control moves, and a client can run one when control
arrives at it or leaves it. Use it for a notification, a wallpaper change, muting audio, or
locking the screen you just left.

**Caps lock indicator**
The caps lock light on your keyboard turns on while another machine is being controlled, so
you can tell at a glance where your typing is going. It is driven directly on the device,
so no display server is involved.

**Ignoring devices**
Devices whose name matches an entry in `ignore-devices` are left alone. Useful for a gaming
mouse, a drawing tablet, or anything you want to stay local to the server.

**Reconnecting**
The client retries on its own with a growing delay when the server goes away, so a server
restart costs you half a second rather than the systemd restart delay.

**Encryption**
All traffic is TLS, using a certificate you generate yourself. Clients additionally prove
they know a shared password before the server sends them anything.

## Requirements

- The uinput kernel module, enabled by default in most distros. Check that `/dev/uinput` exists.
- libevdev development files (`sudo apt install libevdev-dev` on Debian/Ubuntu)
- Clang/LLVM (`sudo apt install clang` on Debian/Ubuntu)

## Installation

Arch users can build the included `PKGBUILD` with `makepkg -si`. Otherwise:

```
$ cargo build --release
# cp target/release/rkvm-client /usr/bin/
# cp target/release/rkvm-server /usr/bin/
# cp target/release/rkvm-certificate-gen /usr/bin/ # Optional
# cp systemd/rkvm-client.service /usr/lib/systemd/system/
# cp systemd/rkvm-server.service /usr/lib/systemd/system/
```

## Setup

1. Generate a certificate and key, listing every address or hostname clients will use:

   ```
   $ rkvm-certificate-gen certificate.pem key.pem -d myserver.local -i 192.168.1.10
   ```

   Put both on the server as `/etc/rkvm/certificate.pem` and `/etc/rkvm/key.pem`.
   Put the certificate alone on every client.

2. Copy `example/server.toml` to `/etc/rkvm/server.toml` on the server, and
   `example/client.toml` to `/etc/rkvm/client.toml` on the clients. A packaged install
   keeps the same files in `/usr/share/rkvm/examples/`, which your package manager
   overwrites, so edit the copies in `/etc/rkvm` rather than those.

3. **Change the password**, and keep the config to yourself. It holds that password
   in plain text, and anyone who can read it can connect and receive everything you type.

   ```
   # chmod 600 /etc/rkvm/server.toml
   ```

4. The server takes over your input devices, so try it before trusting it. This runs it
   for 15 seconds and then gives everything back:

   ```
   # rkvm-server /etc/rkvm/server.toml --shutdown-after 15
   ```

5. Enable the service, `rkvm-server` on the server and `rkvm-client` on the clients:

   ```
   # systemctl enable --now rkvm-server
   ```

## Server configuration

| Option | Description |
| --- | --- |
| `listen` | Address and port to listen on. A hostname works too. |
| `switch-keys` | Keys that, held together, hand control to the next machine. Names are in [switch-keys.md](switch-keys.md). |
| `certificate`, `key` | Paths to the TLS certificate and private key. |
| `password` | Clients have to send this to connect. |
| `propagate-switch-keys` | Whether the switch keys also reach the machine being controlled. Defaults to `true`. Either way, the keys still work normally on their own. |
| `ignore-devices` | Devices whose name contains any of these are never taken over. Case insensitive. |
| `indicator` | `caps-lock` lights the caps lock LED while a client is being controlled. Defaults to `none`. |
| `on-switch` | Command to run on every switch. The target's name arrives in `$1` and `$RKVM_TARGET`. |
| `[switch-to]` | Shortcuts that jump straight to one machine. `server` means this machine, anything else matches a client's `name`. |
| `[clipboard]` | Commands used to share the clipboard, see below. |

```toml
listen = "0.0.0.0:5258"
switch-keys = ["left-alt", "left-ctrl"]
certificate = "/etc/rkvm/certificate.pem"
key = "/etc/rkvm/key.pem"
password = "123456789"

[switch-to]
server = ["left-alt", "escape"]
laptop = ["left-alt", "f1"]
```

## Client configuration

| Option | Description |
| --- | --- |
| `server` | Address or hostname of the server, with its port. |
| `certificate` | Path to the server's TLS certificate. |
| `password` | Has to match the server's. |
| `name` | Name of this machine, so the server can switch straight to it. |
| `on-active` | Command to run when this machine starts or stops being controlled. `true` or `false` arrives in `$1` and `$RKVM_ACTIVE`. |
| `[clipboard]` | Commands used to share the clipboard, see below. |

```toml
server = "myserver.local:5258"
certificate = "/etc/rkvm/certificate.pem"
password = "123456789"
name = "laptop"
```

Anything under a `[table]` header belongs to that table, so keep `[switch-to]` and
`[clipboard]` at the end of the file.

## Clipboard

The same block goes in both configs, on every machine that should take part.
`{type}` is replaced with the MIME type being copied.

```toml
# Wayland
[clipboard]
list-types = "wl-paste --list-types"
read = "wl-paste --no-newline --type {type}"
write = "wl-copy --type {type}"
```

```toml
# X11
[clipboard]
list-types = "xclip -selection clipboard -t TARGETS -o"
read = "xclip -selection clipboard -t {type} -o"
write = "xclip -selection clipboard -t {type} -i"
```

Images are preferred over text when the clipboard holds both. Anything above 4 MB is left
alone. Changes take up to a second to travel, since that is how often the clipboard is read.

## Talking to your desktop from a service

`on-switch`, `on-active` and the clipboard commands run as whoever runs rkvm, which is
usually root, and root has no access to your Wayland or X session. Point them at your own
session explicitly:

```toml
on-switch = "sudo -u yourname WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000 notify-send rkvm \"now on $RKVM_TARGET\""
```

## Finding key names

Run the server with `RUST_LOG=debug` and press the key. The name it logs is the name to put
in the config, written in kebab case: `LeftAlt` becomes `left-alt`.

## Why rkvm and not Barrier/Synergy?

The original author had problems with those programs, namely his keyboard layout (Czech) not
being supported properly, which stems from the fact that they send characters and then try to
translate them back into keycodes. rkvm assumes nothing about your layout, it sends raw
keycodes only.

rkvm also doesn't know or care about X, Wayland or any display server, because it uses the
uinput API with libevdev to read and generate input events.

## Limitations

- Linux only

## Project structure

- `rkvm-server` - server application code
- `rkvm-client` - client application code
- `rkvm-input` - handles reading from and writing to input devices
- `rkvm-net` - network protocol encoding and decoding
- `rkvm-certificate-gen` - certificate generation tool

[Bincode](https://github.com/servo/bincode) is used for encoding of messages on the network
and [Tokio](https://tokio.rs) as an asynchronous runtime.

## Donations

If you find rkvm useful, you can donate to the original author using
[Ko-fi](https://ko-fi.com/htrefil).

## License

[MIT](LICENSE)
