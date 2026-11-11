{
  description = "A minimal Wayland screen locker with a glitch typing indicator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        toolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "clippy"
            "rust-analyzer"
          ];
        };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "glitchlock";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          buildInputs = [ pkgs.pam ];
          meta.mainProgram = "glitchlock";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            toolchain
            pkgs.pam
          ];
        };
      }
    );
}
