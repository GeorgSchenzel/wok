self:
{
  config,
  lib,
  pkgs,
  ...
}:

# The daemon on the GPU machine: socket-activated on
# ~/.local/share/wok/wok.sock (bento's global `wok` channel points there),
# exits when idle. `--show` jobs use WAYLAND_DISPLAY from the systemd user
# manager's environment, looked up per job (sway's session imports it);
# without it they're refused. Each job runs in a transient user scope with
# memory and task caps (systemd-run).
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
    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "8G";
      description = "MemoryMax of each job's scope (its tmpfs counts too).";
    };
    tasksMax = lib.mkOption {
      type = lib.types.int;
      default = 1024;
      description = "TasksMax of each job's scope.";
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
                gnused
                systemd
              ]
            )
          }"
          "WOK_JAIL_PATH=${lib.makeBinPath cfg.jailTools}"
          "WOK_DESKTOP_ENV=systemd"
          "WOK_MEMORY_MAX=${cfg.memoryMax}"
          "WOK_TASKS_MAX=${toString cfg.tasksMax}"
        ];
      };
    };
  };
}
