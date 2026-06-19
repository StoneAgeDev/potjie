#!/bin/sh
# Locate QEMU on the host.  In the Flatpak sandbox, the host filesystem is
# exposed under /run/host, so host binaries live at /run/host/usr/bin (or
# /run/host/bin on some distros).
for d in /run/host/usr/bin /run/host/usr/local/bin /run/host/bin; do
    [ -z "$POTJIE_QEMU_SYSTEM" ] && [ -x "$d/qemu-system-x86_64" ] \
        && export POTJIE_QEMU_SYSTEM="$d/qemu-system-x86_64"
    [ -z "$POTJIE_QEMU_IMG" ] && [ -x "$d/qemu-img" ] \
        && export POTJIE_QEMU_IMG="$d/qemu-img"
done
exec potjie-gtk "$@"
