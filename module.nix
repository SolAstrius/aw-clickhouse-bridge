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
        type = lib.types.nullOr lib.types.path;
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

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Whether to open the firewall for the configured port.";
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/aw-clickhouse-bridge";
      description = "Directory for persistent data (cache, device ID).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.aw-clickhouse-bridge = {
      description = "ActivityWatch to ClickHouse Bridge";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        CLICKHOUSE_URL = cfg.clickhouse.url;
        CLICKHOUSE_DATABASE = cfg.clickhouse.database;
        BIND_ADDR = "${cfg.bindAddr}:${toString cfg.port}";
        AW_DATA_DIR = cfg.dataDir;
      } // lib.optionalAttrs (cfg.clickhouse.user != null) {
        CLICKHOUSE_USER = cfg.clickhouse.user;
      };

      script = ''
        ${lib.optionalString (cfg.clickhouse.passwordFile != null) ''
          export CLICKHOUSE_PASSWORD="$(cat ${lib.escapeShellArg cfg.clickhouse.passwordFile})"
        ''}
        ${lib.optionalString (cfg.clickhouse.passwordFile == null && cfg.clickhouse.password != null) ''
          export CLICKHOUSE_PASSWORD=${lib.escapeShellArg cfg.clickhouse.password}
        ''}
        exec ${lib.getExe cfg.package}
      '';

      serviceConfig = {
        Type = "simple";
        Restart = "always";
        RestartSec = 5;

        DynamicUser = true;
        StateDirectory = "aw-clickhouse-bridge";

        # Hardening
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        PrivateUsers = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
  };
}
