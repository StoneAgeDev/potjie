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
Networking reverted to slirp + opt-in SSH port forwards 2026-06-19 (passt removed).

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
- **Start/stop notifications now live in the daemon** (moved 2026-06-18). The
  daemon's 2 s watchdog detects each running-state transition (any cause — UI,
  CLI, wrapper, or its own sweep) and fires a desktop notification via
  `notify-send`, so notifications happen **even with no GUI open**. Sent on a
  detached thread so a slow/absent notification server can't stall the watchdog;
  the shutdown path sends synchronously. The GUI keeps only a 1 Hz **sidebar-dot**
  poller (`start_sidebar_poller`) — it deliberately does **not** rebuild the
  sidebar (that would drop selection, tear down the detail panel, and stop the box
  we just started) and no longer notifies (would double up with the daemon).
  `notify-send` is bundled via `libnotify` on the daemon's PATH in the flake.
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
- **Launcher management + safe delete (2026-06-18).** Apps tab now has a
  **"Created launchers"** list (`wrappers_section`) showing every wrapper for the
  box (via `desktop::list_wrappers`, identified by the `X-Potjie-Box` marker), each
  with an inline trash button (`desktop::remove_wrapper`); it refreshes when you
  create a new one. **Box delete now asks first** (the inline y/n bar, not a popup)
  and the prompt names how many launchers it will also remove. Cascade lives in
  `Vm::delete` → `desktop::remove_wrappers_for_box`, so the CLI `potjie rm` cleans
  up stale launchers too.
- **Wrapper waits for real SSH before launching the host app.** `wait_for_ssh`
  now requires the `SSH-` identification banner, not just a TCP accept (slirp
  accepts the forwarded port before the guest sshd is up). Since the daemon's
  `acquire` always calls `wait_for_ssh` (even when resuming an already-running
  box), the host app only launches once a real ssh session will succeed — so
  Remote-SSH apps resume cleanly instead of failing the first reconnect.

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
- **slirp user-mode NAT** (`-netdev user`) — user-space, no root, no TAP/bridges.
  QEMU's built-in slirp stack NATs the guest out to the world and forwards a single
  free host port → guest :22 (`hostfwd=tcp:127.0.0.1:<port>-:22`), so Potjie's
  internal SSH always has a conflict-free entry point even on hosts where port 22 is
  occupied. `wire_network()` in `qemu.rs` is now a thin slirp wrapper; the host port
  is recorded in `$XDG_RUNTIME_DIR/potjie/<box>/ssh.port`.

  **Other guest ports** are *not* exposed at the QEMU layer. Instead they are
  tunnelled per-box over SSH, configured by the user and persisted in `box.json`
  (`forwards: [Forward]`). This keeps host port exposure explicit and user-controlled
  rather than opening the whole guest to the host. **Built 2026-06-19**:

  - **Both directions**: `Local` = host→guest (`LocalForward -L`, reach a service
    inside the box) and `Remote` = guest→host (`RemoteForward -R`, expose a host
    service to the box). Listening side always binds loopback (`127.0.0.1`).
  - **Source of truth = the managed ssh config** (`~/.potjie/ssh/config`, the same
    fragment that carries the `potjie-<box>` aliases). Each running box's `Host`
    block now also emits `ControlMaster auto` + a per-box `ControlPath`
    (`$XDG_RUNTIME_DIR/potjie/control-<box>.sock`) + `ControlPersist`, plus one
    `LocalForward`/`RemoteForward` line per forward.
  - **Live, no restart**: while a box runs, the **daemon holds one background SSH
    control master** (`ssh -N -f`, started in `acquire()` after SSH comes up, torn
    down with `ssh -O exit` on every stop path — `release`/`force_stop`/watchdog
    `sweep`/`shutdown`). Editing forwards reconciles the delta against that master
    with `ssh -O forward`/`-O cancel` (`forward::reload`), so changes take effect
    immediately. See `crates/potjie-core/src/forward.rs`.
  - **Surfaces**: daemon IPC `SetForwards`/`GetForwards` (`protocol.rs`,
    `guard::{set,get}_forwards`); CLI `potjie forward list|add|rm`; GUI **Ports** tab
    (Overview/Apps/**Ports**/Shell — `SHELL_PAGE` bumped to 3). Forward edits are
    best-effort: a forward that can't bind never blocks the box
    (`ExitOnForwardFailure no`).

  **History**: 2026-06-18 had a passt mode (all-ports 1:1 via `-t all -u all` over a
  Unix-socket stream netdev). It was **torn out 2026-06-19** — passt's ~5-8 s
  all-ports bind, orphaned-daemon port leaks, and failure to start inside the Flatpak
  sandbox ("timed out waiting for passt to start") made it fragile. slirp + opt-in SSH
  forwards is simpler and more controllable. No passt binary is bundled or probed
  anymore (removed from `flake.nix`, the Flatpak manifest/wrapper, and `tools.rs`).

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
and the alias clears on release. Each generated block also carries
`SetEnv TERM=xterm-256color` so modern terminals (alacritty/kitty/ghostty, whose
`$TERM` the fresh guest has no terminfo for) get a TERM the guest understands.
Verified against OpenSSH 10.3: with a pty, SetEnv overrides the forwarded TERM
(`xterm-kitty` → `xterm-256color`).

### GUI wouldn't launch — wrapper squatted the unique app ID (2026-06-17)
Symptom: `potjie-gtk` exited 0 with no window (the strace was a red herring —
all those `glibc-hwcaps` / `.mo` ENOENTs are benign loader/locale probing).
Cause: a lingering `potjie-gtk --launch …` wrapper (a stuck Zed launcher) owned
the `com.potjie.Potjie` bus name. Wrapper mode built its `adw::Application` with
the unique APP_ID and only `HANDLES_COMMAND_LINE`, so the first wrapper became
the GApplication *primary*; the real GUI then registered as a remote, forwarded
its activation to the stuck wrapper (which has no window-opening `activate`), and
returned. Fix: wrapper mode now also sets `ApplicationFlags::NON_UNIQUE`, so each
`--launch` is its own primary and never squats the shared name. (Immediate
recovery for a stuck instance: kill the lingering `--launch` PID.)

### Shell-tab guard ignored other lease holders (2026-06-17)
The GTK lifecycle was built assuming the Shell tab is the *sole* owner of a box's
lifetime ("the box runs only while the Shell tab is current"). False once a host
app (Zed) leases the same box independently: leaving the Shell tab, closing the
window, or switching boxes would pop the "this stops and re-locks the VM" prompt
and — on "no" — bounce you back to the Shell tab, even though dropping the shell's
lease wouldn't re-lock anything (Zed still held the box). You couldn't navigate
away. Fix: replaced the `box_running` guard with `would_relock`, which checks the
daemon's `BoxStatus.leases` and only treats an action as destructive when the box
is up **and** we hold the only lease (`leases <= 1`; the Shell tab's `potjie ssh`
is itself one lease, so any other holder makes it 2). When other leases exist,
leaving the Shell tab now just closes the ssh session silently — no prompt.

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

## Distribution — DECIDED 2026-06-18: nix bundle (qemu included)
Use **`nix bundle`** to emit one self-contained artifact (real type-2 AppImage via
a bundler flake like `ralismark/nix-appimage`; the default bundler is only an arx
self-extractor). Nix is a *build-time* dep on the dev host; end users get one file
and never touch Nix. This is **not** the torn-down route (`project-nix-packaging-
pivot`): that was distributing a closure/sysext to the host; this is "build with
Nix, ship an opaque blob." Why Nix: it makes the two hard parts free — the GTK
runtime-discovery hell (`wrapGAppsHook`) and bundling **qemu** + its `share/qemu`
firmware/keymap blobs.

**Key resolution — the daemon owns the bundled environment.** A Nix closure needs
literal `/nix/store/<hash>` paths, faked via an unprivileged-userns bind mount that
lives only for the launching process — which collides with "daemon must outlive
the GUI." But `potjied` is already the *sole* spawner of qemu (clients only
Acquire/Release over the lease socket), so **only the daemon needs `/nix/store`.**
So the daemon brings up the namespace/store, `setsid`-detaches, and holds it for
its lifetime; qemu always runs as its child inside that namespace; GUI/CLI never
need the store. Accepted caveat: hard-depends on unprivileged user namespaces —
fine, since anyone who bans userns has also banned `/dev/kvm`/qemu and could never
run Potjie anyway. See memory `project-distribution-decision`.

Env indirection for the bundle (no further code change to point at bundled deps):
- `POTJIE_QEMU_IMG`, `POTJIE_QEMU_SYSTEM` → bundled qemu.
- `POTJIE_BIN` → the multicall `potjie` binary (e.g. the copy materialized into
  `~/.potjie/bin`).

### Multicall binary (done 2026-06-18)
Merged `potjied` into `potjie` as a hidden `potjie daemon` subcommand
(busybox/tmux style): `potjie-daemon` is now a **library** (`run()` entry point)
that `potjie-cli` calls; the standalone `potjied` binary is gone. `potjie-gtk`
stays a separate binary so the daemon never links GTK. `guard::ensure_daemon`
spawns `potjie daemon` (resolved via `tools::potjie_bin()`: `POTJIE_BIN` → sibling
`potjie` → `PATH`), so both the CLI (current_exe = `potjie`) and the GTK app
(sibling `potjie`) spawn it correctly. So distribution ships **two** artifacts:
`potjie` (CLI + daemon) and `potjie-gtk` (GUI). Verified: hidden from `--help`,
steps aside if a daemon is already live, `potjie list` talks to the daemon.

### Idle-self-exit + `$APPIMAGE` launchers + flake (done 2026-06-18)
- **Daemon idle-self-exit:** `potjied` (now `potjie daemon`) exits after
  `POTJIE_DAEMON_IDLE_SECS` (default 90) with zero leases and no client activity.
  Decided on the accept loop's idle tick so it can't race an incoming connection;
  any accepted connection (even a status poll) resets the timer, and the GUI's
  ~1/s status poll keeps it warm while open. `0` disables (run forever).
- **`$APPIMAGE` launchers:** generated `.desktop` wrappers now embed
  `launcher_path()` = `$APPIMAGE` when set, else `current_exe()`. `potjie_cli()`
  in the GUI now delegates to `tools::potjie_bin()` so GUI and daemon-spawn agree
  on which `potjie` to run (`POTJIE_BIN` → sibling → PATH).
- **flake.nix:** one workspace build producing both binaries; `potjie-gtk` wrapped
  via `wrapGAppsHook4` + `--set POTJIE_QEMU_SYSTEM/IMG/BIN` (qemu in closure);
  `potjie` wrapped with `--set-default` for the headless daemon path.
  `meta.mainProgram = potjie-gtk`. Bundle with the `ralismark/nix-appimage`
  bundler — the default bundler is only an arx extractor.

### `--launch` stale-process bugs fixed (2026-06-18)
Two bugs caused lingering `--launch` processes and blocked re-launches:

1. **`--launch` process never exited (bug)** — in `launch_with`, the `!needs_boot`
   branch (box already running) called `parent.set_visible(false)` but never
   `parent.close()`. GTK4 apps don't quit on `drop(hold)` alone when a registered
   `ApplicationWindow` still exists (invisible != unregistered). Fix: `parent.close()`
   added to the `!needs_boot` callback before `drop(hold)`.

2. **Orphaned passt blocks next launch (bug)** — when passt timed out before writing
   its pid file, `kill_passt_for_box` (which reads the pid file) was a no-op. The
   orphaned passt kept holding all ports (via `-t all`), so the next passt spawn
   failed immediately → "timed out waiting for passt to start" on every retry until
   `pkill` cleared things. Fix: added `kill_orphaned_passt()` in `qemu.rs` — scans
   `/proc` for any process whose cmdline references our socket path, kills it. Called
   both at the start of `wire_network` (before spawning new passt) and in the timeout
   cleanup path. **Superseded 2026-06-19**: passt removed entirely (see Networking);
   `kill_orphaned_passt`/`kill_passt_for_box` deleted.

### AppImage built & runs (2026-06-18)
`nix bundle --bundler github:ralismark/nix-appimage .#potjie` → `potjie-gtk.AppImage`,
launches GTK on CachyOS (non-NixOS, no host /nix). Notes:
- **Notifications now via the daemon** (see GTK UX section): `notify-send` from
  `libnotify`, on the daemon's PATH in the flake.
- **"GUI still shows as open after quit" is expected & self-cleaning.** The daemon
  runs from the bundled `/nix/store/…/bin/potjie daemon`, which only exists inside
  the AppImage runtime's userns mount; so the AppImage *runtime* process can't exit
  while the daemon lives in its namespace — it lingers hosting `/nix/store` until
  the daemon idle-exits (90s), then the namespace + runtime tear down. This is the
  "daemon owns the bundled environment" design working for free. Tradeoff: a
  resident runtime + FUSE mount for up to ~90s after closing the GUI.
- **Fontconfig warning spam** on bleeding-edge hosts (bundled fontconfig parsing
  the host's newer `/etc/fonts/conf.d`: `xsi:nil` / `ui-sans-serif` invalid
  constants). Harmless (fonts fall back). Fixed in flake: `--set FONTCONFIG_FILE`
  via `makeFontsConf { fontDirectories = [/usr/share/fonts …]; }` on the GUI
  wrapper — uses version-matched rules but still finds host fonts. Re-bundle to apply.
- **GTK fell back to the cairo (software) renderer → sluggish, esp. on
  energy-saver** (cairo is CPU-bound, so a powersave CPU governor starved it; the
  local `target/release` build used vulkan via the *host* loader). Root cause:
  nixpkgs' `vulkan-loader` searches `/nix/store` paths, not the host's
  `/usr/share/vulkan/icd.d`, so inside the bundle it finds no GPU driver and GTK4
  falls back GL→cairo. **Fix:** bundle `mesa` and set on the GUI wrapper
  `VK_ICD_FILENAMES` (glob of *all* Mesa ICDs → GPU-agnostic: Intel ANV, AMD RADV,
  nouveau…), `__EGL_VENDOR_LIBRARY_DIRS`, `LIBGL_DRIVERS_PATH`. Verified the built
  wrapper bakes the paths (incl. `intel_icd` for this host's Arc/Meteor Lake);
  re-bundle + `GSK_DEBUG=renderer` to confirm vulkan. Mesa↔kernel DRM is a stable
  uABI so bundled Mesa drives the host GPU; if a driver can't init it just falls
  back to cairo (strictly safe). NVIDIA-proprietary hosts would still need the
  matching driver (the hard nixGL-NVIDIA case) — not an issue for Mesa GPUs.

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
- [x] **slirp networking**: free host port → guest :22 for management SSH
- [x] **Port forwards**: bidirectional per-box SSH forwards (host↔guest), persisted in
      `box.json`, applied live via an SSH control master (CLI `potjie forward`, GUI Ports
      tab). Live-apply path needs a booted box; not yet smoke-tested end-to-end.
- [x] **Ports tab UX overhaul**: dropped the confusing listen/destination framing (the
      meaning silently swapped with direction). UI now speaks plain **Host port / Box
      port** with a direction dropdown that flips the arrow, a live sentence describing
      the rule, host→box port auto-mirroring, Enter-to-add, and an inline *Advanced*
      expander for the rarely-used destination host. New `Forward::{from_ports,host_port,
      box_port}` helpers in core; `summary()` now reads `Host :8000 → Box :8080`.
- [x] **Fix flaky add/delete in Ports tab**: the row list was mutated optimistically
      *before* the async daemon write, so a failed write (e.g. a daemon spawn race after
      idle-exit) silently left `box.json` unchanged and the "deleted" forward reappeared
      on the next panel rebuild. Now the daemon write happens first and the in-memory
      list is committed only on success; failures keep the rows and show a loud (`.error`)
      status. Backend verified fine via CLI round-trip (`forward add/list/rm`).
- [ ] Harden: QMP-based status/port (vs
      pidfile), daemon socket peer-cred check, resize/clone/snapshot, per-box
      port-forward config beyond SSH, error surfacing/toasts
- [ ] Persist UI leases across detail-panel rebuilds more explicitly; auto-refresh
- [ ] Tests (unit for seed/config/protocol; integration behind a `--no-kvm` flag)
```
