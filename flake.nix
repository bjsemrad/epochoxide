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
          menus = 2000;
          files = 0;
          clipboard = 0;
        };
        query_prefixes = {
          ">" = "runner";
          "/" = "files";
          "#" = "clipboard";
          "@" = "windows";
          ":" = "menus";
          "?" = "calc";
        };
        icon_theme = "";
        icon_cache_dir = "~/.cache/epochoxide/icons";
        thumbnail_cache_enabled = true;
        persistent_index = true;
      };

      defaultRuntimePackages =
        pkgs: with pkgs; [
          wl-clipboard
          xclip
          xdg-utils
          wmctrl
          tesseract
          libqalculate
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
              source = tomlFormat.generate "epochoxide-config.toml" (defaultSettings // cfg.settings);
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
                Environment = "PATH=${runtimePath}";
                Restart = "on-failure";
                RestartSec = 1;
              };
              Install.WantedBy = [ "default.target" ];
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
          tomlFormat = pkgs.formats.toml { };
          configFile = tomlFormat.generate "epochoxide-config.toml" (defaultSettings // cfg.settings);
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
              wantedBy = [ "default.target" ];
              serviceConfig = {
                Type = "simple";
                ExecStart = "${package}/bin/epochoxide --config ${configFile} serve --socket ${cfg.socket}";
                Environment = "PATH=${runtimePath}";
                Restart = "on-failure";
                RestartSec = 1;
              };
            };
          };
        };
    };
}
