# bmc-tui (proof of concept)

Terminal UI around `ipmitool`, Supermicro first. Screens: Dash, Sensors, Events, FRU, Control (power, identify, clear log, BMC reset), Fan (Supermicro X9/X10 fan mode), plus Serial over LAN. Plan: `DO-NOT-COMMIT/PLAN_bmc_tui_*.md` (not committed).

    IPMI_PASSWORD=... bmc-tui -H 192.168.1.20 -U ADMIN
    bmc-tui --pass-file ~/.bmc-pass -H host -U ADMIN   # file must be mode 0600
    bmc-tui --dump sensors                             # plain text, in-band

Safety: no password on the command line, read-only until `u`, host name typed for risky actions, every write in `~/.local/state/bmc-tui/audit.log`, fan control only on Supermicro BMCs. License undecided.
