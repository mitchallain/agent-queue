{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.agent-queue;
in
{
  options.programs.agent-queue = {
    enable = lib.mkEnableOption "agent-queue, a TUI and CLI queue manager for tmux-hosted AI coding agent sessions";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.agent-queue or (pkgs.callPackage ./package.nix { });
      defaultText = lib.literalExpression "pkgs.agent-queue";
      description = "The agent-queue package to install.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
  };
}
