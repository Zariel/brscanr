{
  description = "Native Brother scanning and Paperless ingestion";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      devShells = eachSystem (system: let pkgs = import nixpkgs { inherit system; }; in {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config curl jq (python3.withPackages (p: [ p.pillow ])) openssl poppler-utils kubectl ];
          shellHook = ''
            export CARGO_HOME="$PWD/.cache/cargo"
          '';
        };
      });
    };
}
