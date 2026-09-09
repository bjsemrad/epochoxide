{
  description = "EpochOxide, a fast Rust desktop shell data provider";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = fn: nixpkgs.lib.genAttrs systems (system: fn nixpkgs.legacyPackages.${system});
      mkPackage =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "epochoxide";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                name = baseNameOf path;
              in
              !(
                type == "directory"
                && builtins.elem name [
                  "target"
                  ".git"
                ]
              );
          };

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "Fast Linux desktop shell data provider";
            homepage = "https://github.com/epochoxide/epochoxide";
            license = pkgs.lib.licenses.mit;
            mainProgram = "epochoxide";
            platforms = pkgs.lib.platforms.linux;
          };
        };

      # Shared between homeManagerModules.default and nixosModules.default so the two
      # deployment paths can't quietly drift apart on defaults.
      defaultSettings = {
        file_roots = [ "~" ];
        ignored_dirs = [
          "~/.cache"
          "~/.local/share/Trash"
          "~/.cargo/registry"
          "~/.rustup"
          "~/.npm"
          "~/.pnpm-store"
          "~/.var/app"
          ".git"
          "node_modules"
          "target"
          "dist"
          "build"
          ".direnv"
        ];
        menus_dir = "~/.config/epochoxide/menus";
        launch_prefix = "";
        terminal_cmd = "";
        clipboard_max_items = 100;
        clipboard_image_dir = "~/.cache/epochoxide/clipboard/images";
        # clipboard_text_editor is deliberately left unset so the daemon's own
        # $EDITOR-sensing default keeps working for Nix-managed installs.
        clipboard_image_editor = "";
        clipboard_ocr = false;
        # Screenshots. The filename is expanded by date(1), so strftime escapes work, and its
        # extension picks the format: .png (default), .jpg, or .ppm.
        screenshot_dir = "~/Pictures/Screenshots";
        screenshot_filename = "screenshot-%Y%m%d-%H%M%S.png";
        screenshot_copy = true;
        screenshot_save = true;
        screenshot_notify = true;
        clipboard_capture_interval_ms = 250;
        runner_scan_path = true;
        runner_commands = [ ];
        provider_enabled = {
          apps = true;
          files = true;
          runner = true;
          clipboard = true;
          windows = true;
          calc = true;
          menus = true;
        };
        provider_weights = {
          apps = 20000;
          runner = 12000;
          calc = 10000;
          windows = 6000;
          files = 0;
          clipboard = 0;
        };
        # Menus take a prefix under their own name (each menu is its own provider), so "?" is
        # left free for one rather than spent on the calculator.
        query_prefixes = {
          ">" = "runner";
          "/" = "files";
          "#" = "clipboard";
          "@" = "windows";
          "=" = "calc";
        };
        icon_theme = "";
        icon_cache_dir = "~/.cache/epochoxide/icons";
        thumbnail_cache_enabled = true;
        persistent_index = true;
        # "auto" searches with fd and only builds the in-memory index when fd is missing.
        # "always" trades memory for latency: <10ms queries instead of ~200ms, at the cost of
        # holding every indexed path in RAM (>1GB over a 700k-entry home directory).
        # "never" keeps the daemon small and relies on fd being installed.
        file_index = "auto";
      };

      defaultRuntimePackages =
        pkgs: with pkgs; [
          wl-clipboard
          xclip
          xdg-utils
          wmctrl
          tesseract
          libqalculate
          imagemagick
          librsvg
          fd
          # Capture. grim and slurp are wlroots screencopy tools rather than compositor-specific
          # ones, so the same pair serves Hyprland, niri, and sway. libnotify supplies
          # notify-send, which is how a finished capture reaches the shell's notification server.
          grim
          slurp
          libnotify
        ];
    in
    {
      packages = forAllSystems (pkgs: {
        default = mkPackage pkgs;
        epochoxide = mkPackage pkgs;
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/epochoxide";
          meta.description = "Run EpochOxide";
        };
      });

      checks = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      homeManagerModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.epochoxide;
          tomlFormat = pkgs.formats.toml { };
          package = cfg.package;
          socket = cfg.socket;
          runtimePath = lib.makeBinPath cfg.runtimePackages;
          # Compositor CLIs (hyprctl, niri, swaymsg) are usually already
          # installed through the NixOS system or user profile rather than
          # declared here. Append those profile bin paths so the windows
          # provider can reach them without making EpochOxide depend on a
          # specific compositor.
          profilePath = "/run/current-system/sw/bin:${config.home.profileDirectory}/bin";
          servicePath = lib.concatStringsSep ":" (lib.filter (p: p != "") [ runtimePath profilePath ]);
        in
        {
          options.programs.epochoxide = {
            enable = lib.mkEnableOption "EpochOxide desktop shell data provider";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "EpochOxide package to install.";
            };
            enableService = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Run EpochOxide as a systemd user daemon.";
            };
            runtimePackages = lib.mkOption {
              type = lib.types.listOf lib.types.package;
              default = defaultRuntimePackages pkgs;
              description = "Runtime tools made available to providers in the user service.";
            };
            socket = lib.mkOption {
              type = lib.types.str;
              default = "%t/epochoxide.sock";
              description = "Socket path for the systemd service. %t expands to XDG_RUNTIME_DIR.";
            };
            settings = lib.mkOption {
              type = tomlFormat.type;
              default = defaultSettings;
              description = "Settings written to ~/.config/epochoxide/config.toml.";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ package ];

            xdg.configFile."epochoxide/config.toml" = {
              source = tomlFormat.generate "epochoxide-config.toml" (lib.recursiveUpdate defaultSettings cfg.settings);
            };

            systemd.user.services.epochoxide = lib.mkIf cfg.enableService {
              Unit = {
                Description = "EpochOxide desktop shell data provider";
                After = [ "graphical-session.target" ];
                PartOf = [ "graphical-session.target" ];
              };
              Service = {
                Type = "simple";
                ExecStart = "${package}/bin/epochoxide serve --socket ${socket}";
                Environment = "PATH=${servicePath}";
                Restart = "on-failure";
                RestartSec = 1;
              };
              Install.WantedBy = [ "graphical-session.target" ];
            };
          };
        };

      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.services.epochoxide;
          package = cfg.package;
          runtimePath = lib.makeBinPath cfg.runtimePackages;
          profilePath = "/run/current-system/sw/bin";
          servicePath = lib.concatStringsSep ":" (lib.filter (p: p != "") [ runtimePath profilePath ]);
          tomlFormat = pkgs.formats.toml { };
          configFile = tomlFormat.generate "epochoxide-config.toml" (lib.recursiveUpdate defaultSettings cfg.settings);
        in
        {
          options.services.epochoxide = {
            enable = lib.mkEnableOption "EpochOxide systemd user daemon";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "EpochOxide package to install.";
            };
            socket = lib.mkOption {
              type = lib.types.str;
              default = "%t/epochoxide.sock";
              description = "Socket path for the user service. %t expands to XDG_RUNTIME_DIR.";
            };
            runtimePackages = lib.mkOption {
              type = lib.types.listOf lib.types.package;
              default = defaultRuntimePackages pkgs;
              description = "Runtime tools made available to providers in the user service.";
            };
            settings = lib.mkOption {
              type = tomlFormat.type;
              default = defaultSettings;
              description = "Settings passed to the user daemon via a generated TOML config.";
            };
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ package ];
            systemd.user.services.epochoxide = {
              description = "EpochOxide desktop shell data provider";
              after = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              wantedBy = [ "graphical-session.target" ];
              serviceConfig = {
                Type = "simple";
                ExecStart = "${package}/bin/epochoxide --config ${configFile} serve --socket ${cfg.socket}";
                Environment = "PATH=${servicePath}";
                Restart = "on-failure";
                RestartSec = 1;
              };
            };
          };
        };
    };
}
