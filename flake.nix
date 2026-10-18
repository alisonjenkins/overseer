{
  description = "Overseer: single-user agentic task orchestrator";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      version = cargoToml.workspace.package.version or "0.1.0";
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          overseer = pkgs.rustPlatform.buildRustPackage {
            pname = "overseer";
            inherit version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./crates
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "overseer-cli" ];
            nativeCheckInputs = [ pkgs.git ];
            doCheck = true;
            preCheck = ''
              export HOME=$(mktemp -d)
              export GIT_CONFIG_NOSYSTEM=1
              export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
            '';
            meta = {
              description = "Overseer: single-user agentic task orchestrator";
              mainProgram = "overseer";
            };
          };
        in
        {
          default = overseer;
          overseer = overseer;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              sqlite
              pkg-config
              git
            ];
          };
        }
      );
    };
}
