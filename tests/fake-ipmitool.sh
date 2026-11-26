#!/bin/sh
# Fake ipmitool for tests. Fails if the secret appears anywhere in argv.
args="$*"
case "$args" in
*s3cret*) echo "fake: password leaked in argv" >&2; exit 9 ;;
esac
case "$args" in
*"sol activate"*)
  # Behaves like a login prompt: answers one line, then ends the session.
  echo "[SOL Session operational.  Use ~? for help]"
  printf 'testbox login: '
  read -r line
  echo "got: $line"
  ;;
*"sol deactivate"*) : ;;
*"chassis power status"*) echo "Chassis Power is on" ;;
*"chassis status"*)
  echo "System Power         : on"
  echo "Power Overload       : false"
  echo "Main Power Fault     : false"
  ;;
*"user list"*)
  printf 'ID  Name\t     Callin  Link Auth\tIPMI Msg   Channel Priv Limit\n'
  echo "1                    true    false      false      Unknown (0x00)"
  echo "2   admin            false   false      true       ADMINISTRATOR"
  echo "3   monitor          true    true       true       USER"
  ;;
*"user set password"*)
  for id; do :; done
  for n in 1 2; do
    stty -echo </dev/tty
    printf 'Password for user %s: ' "$id" >/dev/tty
    IFS= read -r typed </dev/tty
    eval "p$n=\$typed"
    stty echo </dev/tty
  done
  [ "$p1" = "$p2" ] || { echo "Passwords do not match"; exit 1; }
  [ "$p1" = reject ] && { echo "Set User Password command failed (user $id)"; exit 1; }
  [ -n "$FAKE_BMC_PASSWORD_FILE" ] && printf '%s\n' "$p1" > "$FAKE_BMC_PASSWORD_FILE"
  echo "Set User Password command successful (user $id)"
  ;;
*"lan print"*)
  echo "IP Address Source       : DHCP Address"
  echo "IP Address              : 192.0.2.10"
  echo "MAC Address             : 02:00:00:aa:bb:cc"
  ;;
*"mc info"*)
  echo "Device ID                 : 32"
  echo "Manufacturer ID           : 10876"
  ;;
*"sdr elist"*)
  echo "CPU1 Temp        | 01h | ok  |  3.1 | 38 degrees C"
  echo "FAN1             | 41h | ns  |  7.1 | No Reading"
  echo "System Temp      | 02h | cr  |  7.1 | 91 degrees C"
  ;;
*"sel elist"*)
  echo "   1 | 11/22/2026 | 01:02:03 | Temperature #0x30 | Upper Critical going high | Asserted"
  ;;
*"fru print"*)
  echo "FRU Device Description : Builtin FRU Device (ID 0)"
  echo " Board Mfg             : Supermicro"
  ;;
*"raw 0x30 0x45 0x00"*) echo " 01" ;;
*) echo "fake: unknown $args" >&2; exit 1 ;;
esac
