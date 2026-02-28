{
  description = "MBuild";

  nixConfig.bash-prompt = "mbuild";
  nixConfig.bash-prompt-prefix = "[\\[\\033[1;33m\\]";
  nixConfig.bash-prompt-suffix = "\\[\\033[0m\\] \\w]$ ";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      system = "x86_64-linux";

      overlays = [
      ];

      pkgs = import nixpkgs {
        inherit system overlays;
        config = {
          allowUnfree = true;
        };
      };

      vscode = pkgs.vscode-with-extensions.override {
        vscodeExtensions = with pkgs.vscode-extensions; [
          bbenoist.nix
          timonwong.shellcheck
          rust-lang.rust-analyzer
          vadimcn.vscode-lldb
          github.copilot-chat
        ];
      };

    in
    {
      devShells.${system} = rec {
        default =
          with pkgs;
          mkShell {
            packages = [
              vscode
              shellcheck
              nixfmt
              nickel
              nodejs
              python3
              cargo
              rustc
              rustfmt
              rust-analyzer
              podman
              micro
            ];
            shellHook = ''
              echo "nixpkgs: ${nixpkgs}"

              export HOME=$(pwd)
              export EDITOR=micro
              export PATH="$PATH:$HOME/node_modules/.bin"
              export RUST_SRC_PATH="${pkgs.rustPlatform.rustLibSrc}"
            '';
          };
      };
    };
}
