{
  description = "agent-queue: TUI and CLI queue manager for tmux-hosted AI coding agent sessions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

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
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          agent-queue = pkgs.callPackage ./nix/package.nix { };
        in
        {
          default = agent-queue;
          inherit agent-queue;
        }
      );

      overlays.default = final: _prev: {
        agent-queue = final.callPackage ./nix/package.nix { };
      };

      homeManagerModules = {
        default = self.homeManagerModules.agent-queue;
        agent-queue = import ./nix/hm-module.nix;
      };
    };
}
