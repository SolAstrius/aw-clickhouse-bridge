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
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

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
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [ cargo rustc rust-analyzer clippy rustfmt ];
        };
      }
    );
}
