{
  description = "WASI 0.3 TypeScript Component";

  # Replace this local development reference with an exact compiler revision.
  inputs.perry-wit.url = "../";

  outputs = { perry-wit, ... }:
    let
      component = {
        name = "my-task";
        src = ./.;
        entry = "src/index.ts";
      };
    in {
      packages = builtins.mapAttrs (_: sdk: {
        default = sdk.buildComponent component;
      }) perry-wit.lib;
      devShells = builtins.mapAttrs (_: sdk: {
        default = sdk.mkSdkShell component;
      }) perry-wit.lib;
    };
}
