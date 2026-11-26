# tetron-bmctui

A terminal UI for server BMCs: sensors, event log, power control and a serial console in one window, over IPMI. It works with mouse or keyboard, locally or over SSH.

It uses `ipmitool` and is written for Supermicro BMCs first. It is part of the tetron suite and works without tetron.

> **Early development.** Hardware testing is limited to one Supermicro H12 board. Not every action has been verified on hardware. Check the result of anything that changes your server.

## Features

- **Serial console in a pane** (serial over LAN) with BIOS colors, scrollback and zoom.
- **Power menu on top of any screen**, so you can power cycle a server while watching its console.
- **Several servers:** keep a host list and switch with a key or a click.
- **Status screens:** overview, sensors, event log and hardware inventory, with a filter and export to a text file.
- **Power and boot:** power control, identify LED, and boot device for the next boot (network, disk, CD/DVD, BIOS setup).
- **Fan mode** on Supermicro boards.
- **Safe by default:** it starts read-only, dangerous actions ask for confirmation, and actions are written to an audit log.

## Requirements

- Linux
- [ipmitool](https://github.com/ipmitool/ipmitool)
- A BMC with IPMI over LAN enabled, or local access through `/dev/ipmi0`
- Rust 1.91 or newer, only to build from source

## Install

On Debian and Ubuntu (amd64 and arm64), use the [tetronos apt repository](https://tetronos.github.io/apt/). `apt` installs `ipmitool` too, and `apt upgrade` keeps the program current:

    sudo curl -fsSLo /usr/share/keyrings/tetronos-archive-keyring.gpg https://tetronos.github.io/apt/tetronos-archive-keyring.gpg
    sudo curl -fsSLo /etc/apt/sources.list.d/tetronos.sources https://tetronos.github.io/apt/tetronos.sources
    sudo apt update
    sudo apt install tetron-bmctui

It is also in the TetronOS category of [tetron-appstore](https://github.com/tetronos/tetron-appstore).

On other Linux systems, download a binary for x86_64 or aarch64 from the [releases page](https://github.com/tetronos/tetron-bmctui/releases) and install `ipmitool` yourself.

Or build from source:

    git clone https://github.com/tetronos/tetron-bmctui
    cd tetron-bmctui
    cargo build --release

The program is then `target/release/tetron-bmctui`.

## Quick start

Run `tetron-bmctui` with no options. It starts on the **Hosts** screen and contacts nothing until you choose a server. Press `a` or click `[ Add host ]` to add one, then select it and press `Enter` or click `[ Use ]`.

You can also name a server on the command line:

Put the BMC password in a file that only you can read:

    (umask 077; cat > ~/.bmc-pass)     # type the password, press Enter, then Ctrl-D

Connect, using the address and user of your BMC:

    tetron-bmctui -H 192.0.2.10 -U ADMIN --pass-file ~/.bmc-pass

The `IPMI_PASSWORD` environment variable works instead of `--pass-file`. The password is never accepted on the command line. `BMC_HOST` and `BMC_USER` can replace `-H` and `-U`. The list offers this machine's own BMC when it has one, and also when you have no saved servers.

To print one screen as text, for example in a script:

    tetron-bmctui -H 192.0.2.10 -U ADMIN --pass-file ~/.bmc-pass --dump sensors

Run `tetron-bmctui --help` for all options.

## Several servers

**Picking a server:** click its tab in the top bar or press its number. On the **Hosts** screen, click or move to a server to select it, then press `Enter` or click `[ Use ]`. Selecting alone does not contact it.

**Changing a saved password:** select the server on the Hosts screen and press `w` or click `[ Password ]`. Give another password file or type the new password. This changes only what is saved on your computer. **It does not change the password on the BMC.** If the old password was in a file that this program wrote and no other server uses it, that file is deleted. Your own files are never deleted. Nothing logs in with the new password until you ask, for example with `r`.

**Adding a server:** on the Hosts screen, press `a` or click `[ Add host ]`. Move between fields with `Tab` or by clicking. `Enter` goes to the next field and saves from the last one. Give either the path of a password file or the password itself.

A password file that you manage is advised. If you type the password, the program warns you and asks for confirmation, then saves it in a new file in `~/.config/tetron-bmctui/pass/`. That file is **not encrypted**: it has mode 0600, but your account, an administrator or anyone with access to an unencrypted disk or backup can read it. The new entry is added to the end of your host list, and your comments are kept. If you started with `-H`, that server comes first and your saved servers can be picked too. To keep the server given with `-H`, select it and click `[ Save host ]`.

You can also edit the list yourself. List your servers in `~/.config/tetron-bmctui/hosts.toml`:

```toml
[[host]]
name = "web01"
address = "192.0.2.11"
user = "ADMIN"                 # optional
pass_file = "~/.bmc-pass"      # required
interface = "lanplus"          # optional: lanplus or lan

[[host]]
name = "db01"
address = "192.0.2.12"
user = "ADMIN"
pass_file = "~/.bmc-pass-db01"
```

The list and every password file must have mode 0600 (`chmod 600`). The list itself holds no passwords, only the path of each password file.

The default list is loaded at start. With `-H` or `BMC_HOST`, that host comes first and is the one in use. `--hosts FILE` names another list and `--select NAME` chooses the first server. The top bar shows the IP and MAC address of the selected server's BMC when the terminal is wide enough; the Hosts screen shows them in its details too. Only the selected server is contacted, and each tab shows when its state was last checked.

## Keys and mouse

You can click screens, host tabs, rows, buttons and key hints. The mouse wheel scrolls the pane under the pointer. In most terminals, hold Shift while dragging to select text.

| Key | Function |
|---|---|
| `Tab`, `Shift-Tab` | next or previous pane |
| arrows, `j`, `k` | move |
| `Enter` | open a screen, run an action or connect the console |
| `u` | unlock or lock writes |
| `p` | power menu |
| `1` to `9` | switch server |
| `/` | filter sensors, events or inventory; `Esc` clears it |
| `e` | export the screen |
| `a` | add a server (Hosts screen) |
| `w` | change a saved password, or save a `-H` server (Hosts screen) |
| `d` | show or hide details |
| `r` | refresh |
| `l` | Live on or off (Overview and Sensors) |
| `S` | full screen serial console |
| `?` | help |
| `q` | quit |

In a narrow terminal one pane is shown at a time. Open the screen list with `[menu]` or `Tab`.

## Refreshing

A screen gets its data when you open it, when you press `r` and after an action. **Nothing refreshes by itself.** The title shows when the data was read. If a refresh fails, old readings stay and the title says that they were not updated. Power and fan state are never kept after a failure.

To watch readings, turn on **Live** on the Overview or Sensors screen with `l` or `[ Live: off ]`. It refreshes every 30 seconds while that screen is open. It turns off when you leave the screen, switch servers or a refresh fails. `--refresh SECONDS` starts with Live on and sets the interval (5 to 3600).

This program does not monitor your server or send alerts.

## Serial console

Open the **Console** screen and press `Enter`. Unlock when asked. Your keys then go to the server for as long as the console pane has the focus.

Press `Ctrl-]`, then:

| Key | Function |
|---|---|
| `.` | disconnect |
| `z` | zoom to the whole terminal, or back |
| `p` | power menu |
| `[` | scroll back with arrows, `PgUp`, `PgDn` and `Home`; any other key returns |
| `Tab` | leave the console pane |
| `u`, `d`, `1` to `9`, `q`, `?` | lock or unlock, details, switch server, quit, help |
| `Ctrl-]` | send `Ctrl-]` to the server |

Tips:

- **Blank screen after connecting?** Serial over LAN does not repeat earlier output. Press a key to make the server print something. In BIOS setup, an arrow key is safer than `Enter`.
- **Screen cut off?** The console is 100x31 characters, a size BIOS setup often uses. Make your terminal larger or zoom. A terminal of at least 100x31 shows all of it when zoomed. `--console-size` changes the size.
- **Colors:** the console shows the colors of the server. `--console-mono` turns them off.
- **Another session is open?** A BMC normally allows one console session. Press `t` to end the other one, after unlock and confirmation.
- **Escape sequence:** `ipmitool` also ends the session when you type `Enter`, `~`, `.`.
- **Other options:** `--prefix` changes the `Ctrl-]` command key. `--scrollback` sets how many lines are kept (default 50000).

For output to appear, serial over LAN and console redirection must be set up in the server's BMC and BIOS settings. For an operating system console, the system must also use the matching serial port.

## Power and boot

Click `[ Power ]` at the top, or press `p`. In a connected console press `Ctrl-]`, then `p`. The menu shows the server's name, address and power state. `Esc` or `[ Cancel (Esc) ]` closes it.

The "Next boot" actions set the boot device for one boot only. Use `--efi-boot` to request UEFI boot. Server boards differ in how they handle these requests, so watch the console to check the result.

## Fans

Fan modes use Supermicro-specific commands and are available on Supermicro BMCs only. Boards differ. **After changing a mode, check the fan speeds and temperatures.**

## Safety

- The program starts **read-only**. Unlocking asks once and names the server.
- After unlock, only the identify LED runs without confirmation. All other actions ask first, and `Enter` alone does not confirm.
- Switching servers locks writes. While a console is connected, switching asks first because it disconnects.
- Writes lock again after 15 minutes without input. `--relock MINUTES` changes this, and `0` turns it off.
- **A connected console stays usable after this lock.** Anyone at your terminal can type into the server until you disconnect.
- Actions and console sessions are recorded in `~/.local/state/tetron-bmctui/audit.log`. An action is refused if it can not be recorded. Keystrokes are never recorded.
- After an apparent login or connection failure, the program makes no more requests by itself until you press `r`, because repeated wrong passwords can lock a BMC account.
- Text from the BMC and the console pane is filtered before it reaches your terminal. **The full screen console (`S`) is not filtered.**
- Exports are new files that only you can read, in `~/.local/state/tetron-bmctui/exports/` or the folder given with `--export-dir`. They can contain serial numbers and addresses.
- IPMI over LAN has known security weaknesses. Keep BMCs on a private management network or a VPN, never on the open internet.

## Limitations

- IPMI only. There is no Redfish or virtual media support, so installation images can not be mounted.
- The console has no log file, search or copy mode.
- The event log can contain records that `ipmitool` can not name. The details pane says when a record is unknown.
- A "No response to keepalive" console disconnect has been seen once. If the console ends unexpectedly, press `Enter` to connect again.

## Try it without a server

From the source folder, this runs the interface with fake readings. Actions are not sent to any hardware, but they are written to the audit log:

    env -u BMC_HOST -u BMC_USER target/release/tetron-bmctui --ipmitool "$PWD/tests/fake-ipmitool.sh"

On the Hosts screen, select a server (`local` if you have no saved list) and press `Enter`.

If you have a host list, add `XDG_CONFIG_HOME=/nonexistent` after `env -u BMC_HOST -u BMC_USER`.

## Development

    cargo test

The tests use no real BMC. Please report problems on the [issues page](https://github.com/tetronos/tetron-bmctui/issues), with your server board model and BMC firmware version. Remove passwords, addresses and serial numbers first.

## License

[MPL-2.0](LICENSE)
