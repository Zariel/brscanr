{
  description = "Native Brother scanning and Paperless ingestion";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      packages = eachSystem (system: let
        pkgs = import nixpkgs { inherit system; };
        app = pkgs.rustPlatform.buildRustPackage {
          pname = "brscanr";
          version = "0.1.1";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./crates ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = [ "-p" "brscanr" ];
          meta.mainProgram = "brscanr";
        };
      in {
        default = app;
      });
      devShells = eachSystem (system: let pkgs = import nixpkgs { inherit system; }; in {
        release = pkgs.mkShell {
          packages = with pkgs; [ kubernetes-helm (python3.withPackages (p: [ p.pyyaml ])) ];
        };
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config curl jq (python3.withPackages (p: [ p.pillow p.pyyaml ])) openssl poppler-utils kubectl kubeconform kubernetes-helm actionlint hadolint docker-client docker-buildx skopeo yq-go ];
          shellHook = ''
            export CARGO_HOME="$PWD/.cache/cargo"
          '';
        };
      });
    };
}
