{
  description = "Bridge for synchronizing email and tags between JMAP and notmuch";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    utils.url = "github:numtide/flake-utils";

    crane.url = "github:ipetkov/crane";

    pre-commit-hooks-nix = {
      url = "github:cachix/pre-commit-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    utils,
    crane,
    pre-commit-hooks-nix,
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  in
    utils.lib.eachSystem systems (system: let
      pkgs = nixpkgs.legacyPackages."${system}";
      craneLib = crane.mkLib pkgs;
      lib = pkgs.lib;

      mdFilter = path: _type: builtins.match ".*md$" path != null;
      mdOrCargo = path: type:
        (mdFilter path type) || (craneLib.filterCargoSources path type);

      src = lib.cleanSourceWith {
        src = ./.;
        filter = mdOrCargo;
        name = "source";
      };

      common-args = {
        inherit src;
        strictDeps = true;

        propagatedBuildInputs = [pkgs.notmuch];
      };

      cargoArtifacts = craneLib.buildDepsOnly common-args;

      mujmap = craneLib.buildPackage (common-args
        // {
          inherit cargoArtifacts;
        });

      pre-commit-check = hooks:
        pre-commit-hooks-nix.lib.${system}.run {
          src = ./.;

          inherit hooks;
        };
    in rec {
      checks = {
        inherit mujmap;

        mujmap-clippy = craneLib.cargoClippy (common-args
          // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

        mujmap-fmt = craneLib.cargoFmt {
          inherit src;
        };

        pre-commit-check = pre-commit-check {
          alejandra.enable = true;
        };
      };
      packages.mujmap = mujmap;
      packages.default = packages.mujmap;

      apps.mujmap = utils.lib.mkApp {
        drv = packages.mujmap;
      };
      apps.default = apps.mujmap;

      formatter = pkgs.alejandra;

      devShells.default = let
        checks = pre-commit-check {
          alejandra.enable = true;
          #rustfmt.enable = true;
          #clippy.enable = true;
        };
      in
        craneLib.devShell {
          packages = with pkgs; [
            rustfmt
            clippy
          ];
          shellHook = ''
            ${checks.shellHook}
          '';
        };
    })
    // {
      hydraJobs = {
        inherit (self) checks packages devShells;
      };
    };
}
