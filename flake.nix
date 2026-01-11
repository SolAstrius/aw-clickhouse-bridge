{
  description = "ActivityWatch to ClickHouse bridge";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  nixConfig = {
    extra-substituters = [ "https://solastrius.cachix.org" ];
    extra-trusted-public-keys = [ "solastrius.cachix.org-1:MawFli42h9VuWjlURZvxDG+M/tfUbELRwU+QN/6VvdM=" ];
  };

  outputs = { self, nixpkgs, flake-utils }:
    {
      nixosModules.default = import ./module.nix;

      overlays.default = final: prev: {
        aw-clickhouse-bridge = final.rustPlatform.buildRustPackage {
          pname = "aw-clickhouse-bridge";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "aw-models-0.1.0" = "sha256-X9DNYAWcY0yFLwv62OGDp01AaGsDCtElkn5sAFmZkyI=";
            };
          };
          meta = with final.lib; {
            description = "ActivityWatch to ClickHouse bridge";
            homepage = "https://github.com/SolAstrius/aw-clickhouse-bridge";
            license = licenses.mit;
            mainProgram = "aw-clickhouse-bridge";
          };
        };
      };
    } //
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };

        # Android cross-compilation toolchain (prebuilt NDK)
        androidPkgs = pkgs.pkgsCross.aarch64-android-prebuilt;
        androidCC = "${androidPkgs.stdenv.cc}/bin/${androidPkgs.stdenv.cc.targetPrefix}cc";
        androidAR = "${androidPkgs.stdenv.cc.bintools}/bin/${androidPkgs.stdenv.cc.targetPrefix}ar";

        commonArgs = {
          pname = "aw-clickhouse-bridge";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "aw-models-0.1.0" = "sha256-X9DNYAWcY0yFLwv62OGDp01AaGsDCtElkn5sAFmZkyI=";
            };
          };
          meta = with pkgs.lib; {
            description = "ActivityWatch to ClickHouse bridge";
            homepage = "https://github.com/SolAstrius/aw-clickhouse-bridge";
            license = licenses.mit;
            mainProgram = "aw-clickhouse-bridge";
          };
        };
      in
      {
        packages = {
          default = pkgs.rustPlatform.buildRustPackage commonArgs;
          static = pkgs.pkgsStatic.rustPlatform.buildRustPackage commonArgs;
        };

        devShells.default = pkgs.mkShell {
          # Don't include Rust - use system rustup which has Android target
          packages = with pkgs; [ rust-analyzer ];

          # Android cross-compilation environment
          CC_aarch64_linux_android = androidCC;
          AR_aarch64_linux_android = androidAR;
          CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = androidCC;

          # 16KB page alignment for Android 15+ (required for Play Store)
          CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS = "-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384";

          # Webui path for rust-embed (must be set before cargo runs, not via build.rs)
          AW_WEBUI_DIR = "aw-webui/dist";
        };
      }
    );
}
