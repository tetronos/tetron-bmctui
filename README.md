# tetron-bmctui

Terminal UI for server BMCs (IPMI now, Redfish planned). Part of the tetron suite; works without tetron.

**Status: early development.** It wraps `ipmitool` and is written for Supermicro BMCs first. Hardware testing is limited to one Supermicro board, and not every action has been tried on it. Expect changes.

## What works

- Screens: Overview, Sensors, Events, Inventory (FRU), Power (power control, identify LED, clear event log, BMC cold reset) and Fans (Supermicro fan mode).
- Host list: several BMCs in one run, picked from the top bar or the Hosts screen.
- Mouse and keyboard control. Panes adapt to the terminal width.
- Serial over LAN, full screen, through `ipmitool sol activate`.

Fan control uses Supermicro OEM raw commands. Boards differ, so check the mode and the fan speeds after a change. It is not available for other vendors yet.

Not built yet: console in a pane, Redfish, filter, help screen and export.

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
| `d` | show or hide details |
| `r` | refresh |
| `S` | serial console (needs unlock) |
| `q` | quit |

Below 97 columns one pane is shown, and the navigation opens from `[menu]` or with `Tab`.

## Safety

- The program starts read-only. Unlocking asks once and names the host.
- After unlock, power on, identify LED and fan modes run at once. Soft shutdown, power off, cycle, reset, BMC cold reset and clearing the event log also ask for confirmation. `Enter` alone does not confirm.
- Writes lock again after 15 idle minutes. `--relock MINUTES` changes this and `0` turns it off.
- Actions and serial console sessions are recorded in `~/.local/state/tetron-bmctui/audit.log`. An action is refused if the record can not be written. Console keystrokes are not recorded.
- Automatic refresh stops after an apparent login or connection failure, because repeated wrong passwords can lock a BMC account.
- Text from the BMC is filtered to printable ASCII on screen and in `--dump`. **The full screen serial console is not filtered:** output from the host goes to your terminal as it is.

## Tests

    cargo test

The tests use no real BMC.

## License

[MPL-2.0](LICENSE)
