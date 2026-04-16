{
  description = "OrbitBuild - Decentralized P2P Docker build orchestration";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            gcc
            pkg-config
            openssl
            openssl.dev
          ];

          RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
        };
      });
}
