{
  description = "ferref: a CLI/TUI reference manager";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "ferref";
          version = "1.0.0";

          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # ferref shells out to `pdftotext` (poppler-utils) by bare name for
          # PDF text extraction; wrap it onto PATH so that works out of the
          # box for anyone running this via `nix run`/`nix profile install`.
          postFixup = ''
            wrapProgram $out/bin/ferref \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.poppler-utils ]}
          '';

          meta = {
            description = "A CLI/TUI reference manager over a plain SQLite file";
            mainProgram = "ferref";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
        };
      }
    );
}
