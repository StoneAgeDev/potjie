# Potjie — captain's log

> A *potjie* is a cast-iron pot you seal and leave on the coals. Potjie the tool
> seals each of your dev environments inside an encrypted pot that is only ever
> opened while you're actually cooking in it.

Potjie manages **secure, encrypted, user-space virtual machines** ("boxes").
Each box is a LUKS-encrypted qcow2 disk that QEMU decrypts *only while it is
running*; networking is slirp (no root, no TAP); provisioning is cloud-init from
a seed image built in pure Rust. A user-space guard daemon guarantees a box is
running/decrypted **only while a trusted app holds it open**.

Status: **working proof-of-concept, verified end-to-end** (2026-06-17).

---

## The flow (as built)

1. **Add a box** (`+` in the UI, or `potjie create <name>`): downloads + verifies
   the pinned Debian base, then `qemu-img convert`s it into a per-box
   LUKS-encrypted qcow2 keyed by *your* passphrase, and writes a cloud-init seed.
2. **Overview tab / `potjie status`**: shows whether the box is active plus basic
   stats (cpus, memory, disk, ssh, lease count). No start/stop button — lifecycle
   is implicit (see Shell tab). Only a Delete action (disabled while running).
3. **Apps tab**: two collapsible lists (collapsed by default, scan-on-expand) —
   **Host applications** and **Box applications**. Picking one asks for a launcher
   name and writes a host `.desktop` *wrapper* that boots the box, runs the app
   for its lifetime, then re-locks the box. Two flavours:
   - **Box app** (`Kind::Vm`): runs *inside* the box, displayed on the host via
     X-forwarded SSH.
   - **Host app** (`Kind::Host`): a *native host* app runs on the host while the
     box is up and connects *into* it over local SSH — e.g. host VS Code +
     Remote-SSH to the `potjie-<box>` alias. Native UI, code/tooling sealed in the
     encrypted box.
4. **Shell tab / `potjie ssh <name>`**: an **embedded VTE terminal** right in the
   window. Lifecycle is **implicit**: opening the Shell tab boots the box
   (passphrase prompted *in the terminal*), and leaving it re-locks the box. On
   boot the terminal **streams the guest serial console** (`runtime_dir/console.log`,
   tailed by `potjie ssh` on a worker thread while `guard::acquire` runs) plus a
   2-second **heartbeat** so it's never blank during the wait — then the screen
   **clears** and hands over the interactive shell. You get a **desktop
   notification** each time a box starts or stops, the sidebar status dot
   **live-updates in place** (🟢/⚪, no rebuild), and any stop is **guarded by a y/n
   prompt** (below).
5. **Launch the wrapper `.desktop` from your DE/WM** — e.g. wrap VS Code so the
   box is only decrypted while VS Code is open.

The box is **never decrypted while you are not actively using it.**

---

## Architecture

Cargo workspace, four crates:

| Crate | Binary | Role |
|-------|--------|------|
| `potjie-core` | (lib) | The engine: paths, base fetch/verify, LUKS disk creation, cloud-init seed, qemu lifecycle, ssh, desktop wrappers, guard protocol + client. |
| `potjie-cli`  | `potjie`  | CLI + the helper `.desktop` wrappers call. Lifecycle goes through the daemon. |
| `potjie-daemon` | `potjied` | The guard daemon (see below). |
| `potjie-gtk`  | `potjie-gtk` | GTK4 + **libadwaita** UI; also runs wrapper mode `--launch <box> <app-id>`. |

### GTK UX model — implicit lifecycle, no floating windows
The user runs a tiling WM (niri) and **floating/popup windows are unacceptable**,
so the GTK UI is fully inline (see memory `feedback-no-floating-windows`):
- **No start/stop button.** Starting is implicit when the **Shell tab** is the
  current notebook page (its embedded VTE terminal runs `potjie ssh`, which leases
  the box and prompts the passphrase inline); stopping happens when you switch to
  any other tab. Driven by the notebook's **`switch_page`** signal (deterministic),
  with a `connect_unmap` backstop for panel teardown / window hide. Stop `SIGHUP`s
  the shell's process group; the CLI dies, the daemon sees the lease socket close,
  re-locks. (An earlier `map`/`unmap`-based version did **not** reliably fire on
  GTK4 notebook tab-switch, so the `potjie ssh` child leaked and the box stayed up
  with no way to stop it from the UI — fixed by switching to `switch_page`.)
- **Inline y/n stop confirmation, guarding *every* stop path.** Any action that
  would stop a running box first asks via a shared `Confirm` bar — answered with
  `y` / `n` / `Esc` from the keyboard (window-level `EventControllerKey`, Capture
  phase; never need to click). Guards: leaving the Shell tab, **selecting another
  box** in the sidebar, the **`+`** new-box button, and **closing the window**.
  The bar is a `gtk::Overlay` child (floats over content) so revealing it **does
  not reflow/shift the UI** — an earlier in-flow `Revealer` pushed the layout down
  and threw off mouse aim. `y` proceeds (which stops the box via teardown / an
  explicit `shell.stop`), `n`/`Esc` cancels (reverts the tab/selection, keeps it
  running). The currently-shown box is tracked in `current_box`; sidebar reverts
  use a `suppress_sel` flag to swallow the reentrant `row-selected`.
- **Embedded terminal copy/paste:** Ctrl+Shift+C / Ctrl+Shift+V via an
  `EventControllerKey` (`copy_clipboard_format(Format::Text)` / `paste_clipboard`).
- **Overview status is live:** re-read on each switch away from the Shell tab (and
  again 1.5 s later to catch the async stop), updating the Status/SSH rows and the
  Delete button's sensitivity.
- **Start/stop notifications.** A 1 Hz poller over daemon-truth status fires a
  desktop `gio::Notification` on every running-state transition (any cause — UI,
  CLI, or wrapper). It deliberately does **not** rebuild the sidebar (that would
  drop selection, tear down the detail panel, and stop the box we just started).
- **Inline "new box" form.** The `+` button renders a create form *in the detail
  panel*: name, passphrase ×2, **vCPU / memory / disk spin buttons**, and a live
  **creation log** (base-image fetch/verify progress streamed from the worker via
  an `async-channel`, then disk-encrypt/seed status). Not a popup. (Fixed a bug
  where the old modal `prompt_text` fired its `close-request` `None` callback
  before the OK `Some(text)` callback, so the `+` flow silently did nothing.)
- **Removed** the old modal boot dialog (live-log + `verify_decrypted` "wait until
  really decrypted" step — the user found that nonsensical) and the modal seal
  dialog (replaced by the stop notification). `verify_sealed` is still exposed via
  `potjie verify`.

### Default guest user
The single cloud-init user is **`potjie`**, unprivileged but with **passwordless
sudo** (`NOPASSWD:ALL`) — many dev tools dislike running as root, so you get a
normal user that can still escalate without a password prompt. Password login is
locked (`lock_passwd: true`, `ssh_pwauth: false`); only the per-box key works.
Verified: `whoami` → `potjie`, `sudo -n id` → `uid=0`.

### Embedded shell (done)
The Shell tab hosts a **real in-window terminal** via the `vte4` crate
(`vte = { package = "vte4", version = "0.8" }`, against host `vte-2.91-gtk4`
0.84). Its PTY child is `potjie ssh <box>` — lease, passphrase prompt, and
re-lock-on-exit all happen inside the embedded terminal. Install note for this
host: `vte4` had been recorded by pacman while a leftover systemd-sysext overlay
was mounted over `/usr`, so its files vanished on unmerge; a `pacman -R vte4 &&
pacman -S vte4` after unmerging fixed it.

### Theming / dark mode
The UI is an `adw::Application` (libadwaita). It follows the system light/dark
preference automatically via the **XDG settings portal**
(`org.freedesktop.appearance color-scheme`), which is the right approach for
bundling: libadwaita's stylesheet is compiled into the library (no dependency on
host GTK theme files) and the portal is a host D-Bus service reachable from
inside an AppImage/Flatpak sandbox. Plain GTK4 alone does *not* honor the
portal's `color-scheme` (it only keys off the theme *name*), which is why the app
was rendering light under WMs like niri that set `color-scheme=prefer-dark` but
leave the theme name `Adwaita`. Verified `StyleManager::is_dark() == true`.

### Threat model / encryption
- QEMU encrypts qcow2 disks natively in **LUKS** format
  (`encrypt.format=luks`). The key lives only inside the running qemu/qemu-img
  process — **no root daemon, no `cryptsetup` device-mapper mapping.**
- The passphrase is handed to qemu via a `secret` object reading a **0600 file on
  tmpfs** (`$XDG_RUNTIME_DIR`), so it never lands on disk and never appears in
  `argv`/`/proc/<pid>/cmdline`. The file is deleted right after qemu reads it.
- The LUKS key is **per-user, per-box** → we can't ship a pre-encrypted image;
  we ship plaintext and encrypt at create-time. (See base image decision.)

### Networking
- **slirp user-mode** networking only (`-netdev user`). Host→guest SSH is a
  single `hostfwd=tcp:127.0.0.1:<port>-:22`. Zero privileges, no bridges/TAP.

### Provisioning
- **cloud-init NoCloud**, seeded by a FAT image labeled `CIDATA` that we build in
  **pure Rust** (`fatfs`) — no host `genisoimage`/`cloud-localds`. This is what
  lets Potjie run on minimal/immutable hosts.
- Seed creates one passwordless-sudo user trusting a **per-box ed25519 key**
  (generated with the `ssh-key` crate), password auth off.

### The guard daemon (`potjied`) — the lifecycle guarantee
The daemon is the **only** thing that starts boxes. Clients **lease** a box over a
unix socket in the user-only runtime dir. Guarantees enforced:
- A box runs/decrypts **only while ≥1 client holds an open lease connection.**
- When the last lease drops — **including a client crash** (kernel closes the
  socket → daemon sees EOF) — the box is stopped and re-locked.
- A **watchdog** (every 2s) stops any box found running with zero leases (e.g.
  one started out-of-band), and a **startup sweep** locks anything inherited from
  a previous run.
- On daemon shutdown (SIGTERM/SIGINT) it stops every box it manages.

Trusted holders today: `potjie ssh`/`run`/`hold`, the GTK Overview "Start"
button (the UI process holds the lease), and the `--launch` wrapper (holds the
lease for the wrapped app's lifetime).

### Host-app lifetime tracking — full descendant tree (2026-06-17)
A naive wrapper would release the lease the instant the launcher process exits —
but many host apps fork a worker and return immediately (e.g. `code` handing off
to an existing instance), which would re-lock the box out from under a still-open
app. **Unacceptable.** Fixed: host wrappers run the app through
`potjie __run-tracked`, which sets `PR_SET_CHILD_SUBREAPER` and `waitpid(-1)`-reaps
the **entire descendant tree**, holding the lease until the *last* descendant
exits — "all its children and everything." See `potjie-core/src/proc.rs`
(`run_until_descendants_exit`). Verified ~3 s lease hold across plain, double-fork,
and detached-grandchild variants.

### Sealed / encrypted assurance — positive proof, not a message
Both the boot and post-use UIs gather *checked evidence*, never just a comforting
string:
- **Boot (`DecryptionProof`, `verify_decrypted`)**: confirms the disk is LUKS at
  rest (`qemu-img info -U`) *and* authenticates into the live guest reading
  `/etc/machine-id`, `/`'s source and `uname` — only possible if qemu actually
  decrypted it.
- **Post-use (`SealProof`, `verify_sealed` / `potjie verify`)**: confirms the box
  is **sealed/encrypted again** — disk LUKS at rest, **no** qemu decrypting it
  (scanned from `/proc`, not just our pidfile), SSH port **unreachable**, and **no**
  leftover key material in the runtime dir. The UI shows a per-check ✓/✗ verdict;
  the CLI exits non-zero if not sealed. This is the "it's locked, 100% sure"
  guarantee at the end of a session.

### Stable SSH alias for host apps
So a host app can reach a box at a fixed name even though the slirp port changes
each boot, the daemon maintains `~/.potjie/ssh/config` with a `Host potjie-<box>`
block for every *running* box, and prepends a single `Include` line to
`~/.ssh/config` (idempotent). `ssh potjie-<box>` / VS Code Remote-SSH
`potjie-<box>` work while the box is up; the alias is removed when it stops.
Host wrappers also export `POTJIE_BOX` / `POTJIE_SSH_{ALIAS,HOST,PORT,USER}` to
the launched app. Verified: `ssh potjie-spike` (alias only) reaches the guest,
and the alias clears on release.

---

## Base image decision (2026-06-17)
Chosen: **pin an official Debian cloud image + cloud-init** (over baking/hosting
our own golden snapshot). Reproducible, nothing to host, swappable later behind
the identical convert-to-LUKS step.

Pinned in `config.rs::DEFAULT_BASE`:
- Debian 13 (trixie) `genericcloud` amd64, dated build **20260601-2496** (~323 MiB)
- verified by **SHA-512** against Debian's `SHA512SUMS`.
- "Update occasionally" = bump the three fields in `DEFAULT_BASE`.

A golden-image path (option B) can later replace the base without touching any
other code.

---

## What's verified (2026-06-17)
- `potjie create spike` → base downloaded + sha512-verified, disk is
  `encrypted: yes / format: luks`, seed is FAT `CIDATA`. ✅
- Boot → cloud-init created user `potjie`, hostname `spike`, key auth over slirp
  forward, passwordless sudo (uid=0), Debian 13 trixie. ✅
- Graceful stop via QMP `system_powerdown` → re-locked, no qemu left. ✅
- Daemon: `list` auto-spawns it; `run` acquires lease → boots → runs → releases →
  box re-locks (`running:false leases:0`). ✅
- **Crash safety:** `kill -9` on a holder re-locks the box within seconds. ✅
- GTK4 app launches and runs its main loop under Wayland. ✅

Leftover test artifact: a box named **`spike`** exists under `~/.potjie/img/spike`
(passphrase `spiketest`). Delete with `potjie rm spike` if unwanted.

---

## Layout on disk
```
~/.potjie/
  base/<pinned>.qcow2            cached, verified base (plaintext, shared)
  img/<box>/box.json             metadata
           disk.qcow2            per-box LUKS root disk
           seed.img              cloud-init CIDATA seed (FAT)
           id_ed25519[.pub]      per-box ssh key
$XDG_RUNTIME_DIR/potjie/         tmpfs: secrets, pidfiles, qmp/ctl sockets, logs
  potjied.sock                   guard daemon control socket
~/.local/share/applications/     generated wrapper .desktop files
```

---

## Distribution — STILL OPEN (next decision)
Goal: one self-contained blob that works anywhere, incl. immutable distros, fully
user-space. Nix-hermetic-closure was torn down earlier as too painful (see memory
`project-nix-packaging-pivot`); don't resurrect it.

Build already supports bundling via env indirection — no code change needed to
point at bundled binaries:
- `POTJIE_QEMU_IMG`, `POTJIE_QEMU_SYSTEM` → bundled qemu.
- `POTJIE_DAEMON` → bundled `potjied`.

Leading candidates (decide later):
- **Flatpak** — best fit for immutable distros (Silverblue/Kinoita), can bundle
  qemu via the freedesktop runtime; sandbox needs `/dev/kvm` + the host SSH/X.
- **AppImage** — single executable blob bundling qemu + the three binaries.

Open sub-questions: how the wrapper `.desktop` files reference the (bundled)
launcher path, and KVM/`/dev/kvm` access inside a Flatpak sandbox.

---

## Roadmap
- [x] POC: pinned base → LUKS disk → cloud-init seed → boot → slirp SSH → stop
- [x] CLI spike (`create/list/status/ssh/run/hold/down/rm`)
- [x] Guard daemon: lease-gated lifecycle + watchdog + crash-safety
- [x] GTK4 UI: sidebar + Overview / Apps / Shell tabs, async (no UI blocking)
- [x] `.desktop` wrapper generation (host + VM apps) + `--launch` foreground flow
- [x] Stable `potjie-<box>` SSH alias for host apps (daemon-maintained); verified
- [x] Host-app **full descendant-tree** lifetime tracking (`__run-tracked` +
      subreaper) — fork/return-immediately apps no longer drop the lease early
- [x] Boot UI (live logs + passphrase) and **checked** decryption proof
- [x] Post-use **sealed/encrypted assurance** (`SealProof` / `potjie verify`)
- [x] **Embedded in-window shell** (VTE) with implicit start/stop on Shell-tab
      visibility + desktop notifications on start/stop
- [x] Inline new-box form (fixed the broken `+` button); removed all modal/popup
      windows per the tiling-WM user's preference
- [ ] Guest/VM app descendant-tracking parity (host path done; VM path still uses
      foreground X-forwarded SSH)
- [ ] **Distribution**: pick Flatpak vs AppImage; bundle qemu + binaries
- [ ] Run a wrapper `.desktop` for a real GUI app end-to-end (X-forward a box app;
      host VS Code Remote-SSH to a box).
- [ ] Harden: QMP-based status/port (vs
      pidfile), daemon socket peer-cred check, resize/clone/snapshot, per-box
      port-forward config beyond SSH, error surfacing/toasts
- [ ] Persist UI leases across detail-panel rebuilds more explicitly; auto-refresh
- [ ] Tests (unit for seed/config/protocol; integration behind a `--no-kvm` flag)
```
