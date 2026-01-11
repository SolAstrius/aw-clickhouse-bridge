{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.aw-clickhouse-bridge;
in
{
  options.services.aw-clickhouse-bridge = {
    enable = lib.mkEnableOption "ActivityWatch to ClickHouse bridge";

    package = lib.mkPackageOption pkgs "aw-clickhouse-bridge" { };

    clickhouse = {
      url = lib.mkOption {
        type = lib.types.str;
        default = "http://localhost:8123";
        description = "ClickHouse HTTP endpoint URL.";
      };

      database = lib.mkOption {
        type = lib.types.str;
        default = "activitywatch";
        description = "ClickHouse database name.";
      };

      user = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "ClickHouse username.";
      };

      password = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          ClickHouse password. Ends up in the Nix store — prefer passwordFile for secrets.
        '';
      };

      passwordFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Path to a file containing the ClickHouse password.
          Takes precedence over password if both are set.
        '';
      };
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 5600;
      description = "Port to listen on.";
    };

    bindAddr = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = ''
        Address to bind to. Can be a comma-separated list of addresses.
        Use "0.0.0.0" to listen on all interfaces.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.dataHome}/aw-clickhouse-bridge";
      defaultText = lib.literalExpression ''"''${config.xdg.dataHome}/aw-clickhouse-bridge"'';
      description = "Directory for persistent data (cache, device ID).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.aw-clickhouse-bridge = {
      Unit = {
        Description = "ActivityWatch to ClickHouse Bridge";
        After = [ "network.target" ];
      };

      Service = {
        Type = "simple";
        Restart = "always";
        RestartSec = 5;

        Environment = [
          "CLICKHOUSE_URL=${cfg.clickhouse.url}"
          "CLICKHOUSE_DATABASE=${cfg.clickhouse.database}"
          "BIND_ADDR=${cfg.bindAddr}:${toString cfg.port}"
          "AW_DATA_DIR=${cfg.dataDir}"
        ] ++ lib.optional (cfg.clickhouse.user != null) "CLICKHOUSE_USER=${cfg.clickhouse.user}";

        ExecStart = let
          script = pkgs.writeShellScript "aw-clickhouse-bridge-start" ''
            ${lib.optionalString (cfg.clickhouse.passwordFile != null) ''
              export CLICKHOUSE_PASSWORD="$(cat ${lib.escapeShellArg cfg.clickhouse.passwordFile})"
            ''}
            ${lib.optionalString (cfg.clickhouse.passwordFile == null && cfg.clickhouse.password != null) ''
              export CLICKHOUSE_PASSWORD=${lib.escapeShellArg cfg.clickhouse.password}
            ''}
            exec ${lib.getExe cfg.package}
          '';
        in "${script}";
      };

      Install = {
        WantedBy = [ "default.target" ];
      };
    };
  };
}
