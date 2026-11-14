self:
{
  config,
  lib,
  pkgs,
  ...
}:

# The daemon on the GPU machine: socket-activated on
# ~/.local/share/wok/wok.sock (bento's global `wok` channel points there),
# exits when idle.
let
  cfg = config.services.wok;
in
{
  options.services.wok = {
    enable = lib.mkEnableOption "the wok GPU job daemon";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.wok;
    };
    jailTools = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = with pkgs; [
        sway
        grim
        coreutils
        bash
      ];
      description = ''
        First on every job's PATH (before the system and user profiles).
        sway and grim are the job's compositor and screenshots.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.sockets.wok = {
      Unit.Description = "wok GPU jobs (socket)";
      Socket = {
        ListenStream = "%h/.local/share/wok/wok.sock";
        SocketMode = "0600";
        DirectoryMode = "0700";
      };
      Install.WantedBy = [ "sockets.target" ];
    };

    systemd.user.services.wok = {
      Unit = {
        Description = "wok GPU jobs";
        Requires = [ "wok.socket" ];
        After = [ "wok.socket" ];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Environment = [
          "PATH=${
            lib.makeBinPath (
              with pkgs;
              [
                bash
                bubblewrap
                coreutils
              ]
            )
          }"
          "WOK_JAIL_PATH=${lib.makeBinPath cfg.jailTools}"
        ];
      };
    };
  };
}
