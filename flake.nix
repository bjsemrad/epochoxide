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
              default = [
                pkgs.wl-clipboard
                pkgs.xclip
                pkgs.xdg-utils
                pkgs.wmctrl
                pkgs.tesseract
                pkgs.libqalculate
              ];
              description = "Runtime tools made available to providers in the user service.";
            };
            socket = lib.mkOption {
              type = lib.types.str;
              default = "%t/epochoxide.sock";
              description = "Socket path for the systemd service. %t expands to XDG_RUNTIME_DIR.";
            };
            settings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              description = "Settings written to ~/.config/epochoxide/config.toml.";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ package ];

            xdg.configFile."epochoxide/config.toml" = lib.mkIf (cfg.settings != { }) {
              source = tomlFormat.generate "epochoxide-config.toml" cfg.settings;
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
              default = [
                pkgs.wl-clipboard
                pkgs.xclip
                pkgs.xdg-utils
                pkgs.wmctrl
                pkgs.tesseract
                pkgs.libqalculate
              ];
              description = "Runtime tools made available to providers in the user service.";
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
                ExecStart = "${package}/bin/epochoxide serve --socket ${cfg.socket}";
                Environment = "PATH=${runtimePath}";
                Restart = "on-failure";
                RestartSec = 1;
              };
            };
          };
        };
    };
}
