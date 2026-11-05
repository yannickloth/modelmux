{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.modelmux;
in
{
  options.services.modelmux = {
    enable = lib.mkEnableOption "modelmux, a rule-based LLM routing proxy";

    package = lib.mkOption {
      type = lib.types.package;
      description = "modelmux package (from the modelmux flake outputs)";
      example = literalExpression "modelmuxFlake.packages.${system}.default";
    };

    config = lib.mkOption {
      type = lib.types.path;
      description = "Path to modelmux.toml";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Environment file (KEY=VALUE lines) that holds the provider API keys.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.modelmux = {
      description = "modelmux LLM routing proxy";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig =
        {
          ExecStart = "${cfg.package}/bin/modelmux --config ${cfg.config}";
          Restart = "on-failure";
          RestartSec = 2;
        }
        // lib.optionalAttrs (cfg.environmentFile != null) {
          EnvironmentFile = cfg.environmentFile;
        };
    };
  };
}
