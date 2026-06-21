{
  description = "Potjie — secure, encrypted, user-space VMs (qemu-luks + cloud-init)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      # qemu-system-x86_64 is the guest, but the *host* arches we can build on:
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        # One workspace build that produces BOTH artifacts:
        #   * potjie      — CLI + `potjie daemon` (pure-Rust, no GUI libs)
        #   * potjie-gtk  — the GTK4 front-end (wrapped for GTK at runtime)
        # We build once and wrap only the GUI, so the daemon stays GTK-free.
        potjie = pkgs.rustPlatform.buildRustPackage {
          pname = "potjie";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [
            pkg-config
            wrapGAppsHook4 # gathers GTK env into gappsWrapperArgs (we wrap manually)
          ];
          # System libs the GTK crate links against. The CLI/daemon need none.
          buildInputs = with pkgs; [
            glib
            gtk4
            libadwaita
            vte-gtk4
          ];

          # Let wrapGAppsHook4 collect its env (schemas, pixbuf loaders, gio
          # modules) but don't auto-wrap — we only want it on the GUI binary, plus
          # our own runtime indirection so the bundle is self-contained:
          #   POTJIE_QEMU_SYSTEM / POTJIE_QEMU_IMG → the bundled qemu
          #   POTJIE_BIN                           → the bundled multicall `potjie`
          dontWrapGApps = true;
          postFixup = ''
            # Point GTK's bundled GL/Vulkan loaders at the bundled Mesa. nixpkgs'
            # vulkan-loader searches /nix/store paths, not the host's
            # /usr/share/vulkan/icd.d, so on a non-NixOS host it finds no driver
            # and GTK4 falls back to the (CPU-bound, sluggish) cairo renderer. We
            # bundle Mesa and list *all* its ICDs so any host GPU (Intel ANV, AMD
            # RADV, nouveau, …) gets hardware acceleration; if a driver can't init
            # it still just falls back to cairo, so this is strictly safe.
            vkicds=$(echo ${pkgs.mesa}/share/vulkan/icd.d/*.json | tr ' ' ':')
            wrapProgram $out/bin/potjie-gtk \
              "''${gappsWrapperArgs[@]}" \
              --set GDK_BACKEND wayland,x11 \
              --set POTJIE_QEMU_SYSTEM ${pkgs.qemu}/bin/qemu-system-x86_64 \
              --set POTJIE_QEMU_IMG ${pkgs.qemu}/bin/qemu-img \
              --set POTJIE_BIN $out/bin/potjie \
              --set VK_ICD_FILENAMES "$vkicds" \
              --set __EGL_VENDOR_LIBRARY_DIRS ${pkgs.mesa}/share/glvnd/egl_vendor.d \
              --set LIBGL_DRIVERS_PATH ${pkgs.mesa}/lib/dri \
              --set FONTCONFIG_FILE ${pkgs.makeFontsConf {
                # Use our own (version-matched) fontconfig rules instead of the
                # host's /etc/fonts, which on bleeding-edge hosts uses newer
                # syntax the bundled fontconfig can't parse (the xsi:nil /
                # ui-sans-serif warnings). Still points at the host's font *dirs*
                # so the user's actual fonts are found.
                fontDirectories = [ "/usr/share/fonts" "/usr/local/share/fonts" ];
              }}
          '';

          # The daemon (inside `potjie`) is the sole spawner of qemu, so it also
          # needs to find the bundled qemu when spawned headless. Point it there
          # too — harmless for the pure-CLI subcommands.
          postInstall = ''
            wrapProgram $out/bin/potjie \
              --set-default POTJIE_QEMU_SYSTEM ${pkgs.qemu}/bin/qemu-system-x86_64 \
              --set-default POTJIE_QEMU_IMG ${pkgs.qemu}/bin/qemu-img \
              --set-default POTJIE_BIN $out/bin/potjie \
              --prefix PATH : ${pkgs.libnotify}/bin
          '';

          meta = {
            description = "Secure, encrypted, user-space VMs";
            mainProgram = "potjie-gtk";
          };
        };

        default = potjie;
      });

      # `nix build .#potjie` then `nix bundle .#potjie` (use a real-AppImage
      # bundler flake, e.g. `nix bundle --bundler github:ralismark/nix-appimage .#potjie`;
      # the default bundler only makes an arx self-extractor).
      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.system}.potjie}/bin/potjie-gtk";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.system}.potjie ];
          packages = with pkgs; [ cargo rustc rustfmt clippy qemu ];
        };
      });
    };
}
