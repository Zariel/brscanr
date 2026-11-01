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
          version = "0.1.0";
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
        image = pkgs.dockerTools.buildLayeredImage {
          name = "brscanr";
          tag = "0.1.0";
          contents = [ app pkgs.cacert ];
          config = {
            Entrypoint = [ "${app}/bin/brscanr" ];
            User = "10001:10001";
            Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
            ExposedPorts = { "8080/tcp" = {}; "54925/udp" = {}; };
          };
        };
      });
      devShells = eachSystem (system: let pkgs = import nixpkgs { inherit system; }; in {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config curl jq (python3.withPackages (p: [ p.pillow ])) openssl poppler-utils kubectl kubeconform ];
          shellHook = ''
            export CARGO_HOME="$PWD/.cache/cargo"
          '';
        };
      });
    };
}
