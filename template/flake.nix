{
  description = "WASI Preview 2 TypeScript Component";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    perry-wit.url = "github:<ORG-TBD>/perry-wit";
  };

  outputs = { self, nixpkgs, flake-utils, perry-wit }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages.default = perry-wit.lib.${system}.buildComponent {
          name = "my-task";
          src = ./.;
          entry = "src/index.ts";
          wit = ./wit;
          world = "task";
        };

        devShells.default = perry-wit.devShells.${system}.default;
      }
    );
}
