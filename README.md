# Potjie

**Plug-and-play secure dev boxes.**

A *potjie* is a cast-iron pot you seal and leave on the coals. Potjie is a tool which keeps each of your dev environments sealed in an encrypted pot that only opens while you're actively working in it.

Each box is a LUKS-encrypted QEMU virtual machine. The disk is decrypted only while a shell tab is open or an ssh session is open.

![Potjie overview screenshot](https://raw.githubusercontent.com/StoneAgeDev/potjie/assets/screenshot-overview.png)

## Features

- LUKS-encrypted QEMU VMs: encryption at rest, no root required
- User-space networking via slirp: no TAP devices or bridging
- Implicit lifecycle: The box runs only while the Shell tab or ssh session is open, re-locks on close
- SSH port forwards configured live in the GUI
- Desktop notifications on every box start and stop

## Install

Potjie is distributed as a Flatpak.

<!-- TODO: add Flathub badge once listed -->

Or build from source:

```bash
flatpak-builder --user --install --force-clean build-dir flatpak/io.github.StoneAgeDev.potjie.yaml
```

## License

MIT
