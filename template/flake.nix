{
  description = "WASI 0.3 TypeScript Component";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Set this to the compiler flake reference for your project.
    perry-wit.url = "../";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    perry-wit,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: {
        packages.default = perry-wit.lib.${system}.buildComponent {
          name = "my-task";
          src = ./.;
          entry = "src/index.ts";
          wit = ./wit;
          world = "task";
        };

        devShells.default = perry-wit.devShells.${system}.sdk;
      }
    );
}
