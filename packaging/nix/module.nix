# NixOS module: installs the package and runs the mount daemon in every
# graphical session, as the other packages' user service does.
#
#   services.proton-drive-linux.enable = true;
self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.proton-drive-linux;
in
{
  options.services.proton-drive-linux = {
    enable = lib.mkEnableOption "the Proton Drive for Linux mount daemon, app and tray";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.proton-drive-linux;
      defaultText = lib.literalExpression "proton-drive-linux.packages.\${system}.proton-drive-linux";
      description = "The package to use.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    # The unit ships in the package; this links it and pulls it into the
    # graphical session, which is what `systemctl --user enable` would do.
    systemd.packages = [ cfg.package ];
    systemd.user.services.proton-drive = {
      wantedBy = [ "graphical-session.target" ];
      # The mount runs fusermount3 from PATH, and it has to be the setuid
      # wrapper; the PATH NixOS gives a unit does not include it.
      path = [ "/run/wrappers" ];
    };
    programs.fuse.enable = lib.mkDefault true;
    # Session tokens are kept in the Secret Service.
    services.gnome.gnome-keyring.enable = lib.mkDefault true;
  };
}
