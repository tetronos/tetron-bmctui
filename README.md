# tetron-bmctui

Terminal UI for server BMCs (IPMI now, Redfish planned). Part of the tetron suite; works without tetron.

**Status: early development.** It wraps `ipmitool` and is written for Supermicro BMCs first. Hardware testing is limited to one Supermicro board, and not every action has been tried on it. Expect changes.

## What works

- Screens: Overview, Sensors, Events, Inventory (FRU), Power (power control, identify LED, clear event log, BMC cold reset) and Fans (Supermicro fan mode).
- Host list: several BMCs in one run, picked from the top bar or the Hosts screen.
- Filter on Sensors, Events and Inventory, a help window, and export of a screen to a text file.
- One-time boot device (PXE, disk, CD/DVD, BIOS setup) on the Power screen.
- Mouse and keyboard control. Panes adapt to the terminal width.
- Serial console in a pane with scrollback and zoom, through `ipmitool sol activate`. `S` runs it full screen instead.

Fan control uses Supermicro OEM raw commands. Boards differ, so check the mode and the fan speeds after a change. It is not available for other vendors yet.

Not built yet: Redfish and virtual media.

## Requirements

Linux, [ipmitool](https://github.com/ipmitool/ipmitool), and Rust 1.91 or newer to build.

## Build and run

    cargo build --release
    target/release/tetron-bmctui -H 192.168.1.20 -U ADMIN --pass-file ~/.bmc-pass

The password file holds only the password and must have mode 0600. The `IPMI_PASSWORD` environment variable works instead of `--pass-file`. The password is never accepted on the command line. Without `-H`, `ipmitool` talks to the local BMC. `BMC_HOST` and `BMC_USER` can replace `-H` and `-U`.

    tetron-bmctui --dump sensors    # print one screen as text and exit

## Host list

To work with several BMCs, put them in `~/.config/tetron-bmctui/hosts.toml` (mode 0600). It is used when neither `-H` nor `BMC_HOST` is given. `--hosts FILE` names another file and `--select NAME` picks the first host.

```toml
[[host]]
name = "testbox"
address = "192.168.1.20"
user = "ADMIN"                 # optional
pass_file = "~/.bmc-pass"      # required, mode 0600
interface = "lanplus"          # optional: lanplus or lan
```

The file holds no passwords. Every host needs its own `pass_file` entry, and `IPMI_PASSWORD` is not used with a list. Only the selected host is contacted. Switching hosts locks writes.

## Without a BMC

To look at the interface without a BMC, run from the repository directory (readings are fake and actions report a failure, but they are still written to the audit log):

    env -u BMC_HOST -u BMC_USER target/release/tetron-bmctui --ipmitool "$PWD/tests/fake-ipmitool.sh"

## Controls

Click screens, rows, buttons and key hints. The wheel scrolls the pane under the pointer. Hold Shift while dragging to select text, in terminals that support it.

| Key | Function |
|---|---|
| `1` to `9` | switch host |
| `Tab`, `Shift-Tab` | next or previous pane |
| arrows, `j`, `k` | move in the focused pane |
| `Enter` | open a screen or run the selected action |
| `u` | unlock or lock writes |
| `p` | power menu |
| `d` | show or hide details |
| `/` | filter lines, `Enter` keeps it, `Esc` clears it |
| `e` | export the screen, as filtered |
| `?` | help |
| `r` | refresh |
| `S` | serial console, full screen (needs unlock) |
| `q` | quit |

Below 97 columns one pane is shown, and the navigation opens from `[menu]` or with `Tab`.

## Console

Open the Console screen and press `Enter` or click `[ Connect (Enter) ]`. Connecting needs unlock. After that, all keys go to the host while the console pane has the focus, with no further prompt. Click another pane, or use the command key `Ctrl-]` followed by:

| Key | Function |
|---|---|
| `.` | disconnect |
| `z` | zoom to the whole terminal, or back |
| `p` | power menu |
| `[` | scroll back with arrows, `PgUp`, `PgDn`, `Home`; any other key returns |
| `Tab` | next pane |
| `u`, `d`, `1` to `9`, `q`, `?` | lock or unlock, details, host, quit, list commands |
| `Ctrl-]` | send `Ctrl-]` to the host |

The mouse wheel scrolls back at any time. Mouse events are not sent to the host. The screen is emulated at 100x31, which AMI BIOS setup commonly draws over serial; 80x24 output fits inside it. A smaller pane shows the part around the cursor and is marked as cropped, so make the terminal larger or zoom (a terminal of at least 100x31 shows all of it zoomed). Serial over LAN does not replay earlier output: after connecting, the screen stays blank until the host prints, so press an arrow key in a quiet BIOS menu. `--console-size`, `--scrollback` (default 50000 lines) and `--prefix` change this. `ipmitool` keeps its own escape: `Enter`, `~`, `.` also ends the session.

If another session holds the console, `t` offers to end it, after unlock and confirmation. Not built yet: console log, search, copy, reconnect and an on-screen panel for function keys. On real hardware it has been tried on one Supermicro H12 board.

## Power menu

`[ Power ]` in the top bar, or `p`, opens the power actions on top of the current screen. In a connected console use `Ctrl-]` then `p`. The console stays connected and visible around the menu, and no keys reach the host while it is open. The menu names the host and reads its power state each time it opens. The rules are those of the Power screen. `Esc` or `[ Cancel (Esc) ]` closes the menu, or steps back to it from a confirmation. Clicks outside it do nothing.

## Events, export and next boot

Exports are new files with mode 0600 in `~/.local/state/tetron-bmctui/exports/`, or in `--export-dir`. They can contain serial numbers and addresses.

In the event log, data of an OEM record is also shown as text when it is readable UTF-16. Records that `ipmitool` can not name stay unnamed, and the details pane says so.

The "Next boot" actions run `ipmitool chassis bootdev`, which applies to one boot. `--efi-boot` adds `options=efiboot`. Boards differ in how they treat this, and it is not yet tested on hardware.

## Safety

- The program starts read-only. Unlocking asks once and names the host.
- After unlock, power on, identify LED and fan modes run at once. Soft shutdown, power off, cycle, reset, BMC cold reset, clearing the event log and the next boot actions also ask for confirmation. `Enter` alone does not confirm.
- Writes lock again after 15 idle minutes. `--relock MINUTES` changes this and `0` turns it off. **A connected console is not locked by this:** anyone at your terminal can keep typing into the host until you disconnect.
- Actions and serial console sessions are recorded in `~/.local/state/tetron-bmctui/audit.log`. An action is refused if the record can not be written. Console keystrokes are not recorded.
- Automatic refresh stops after an apparent login or connection failure, because repeated wrong passwords can lock a BMC account.
- Text from the BMC is filtered to printable ASCII on screen and in `--dump`. The console pane draws only what its terminal emulator parsed. **The full screen serial console (`S`) is not filtered:** output from the host goes to your terminal as it is.

## Tests

    cargo test

The tests use no real BMC.

## License

[MPL-2.0](LICENSE)
