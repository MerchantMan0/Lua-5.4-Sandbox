{
  description = "Lua 5.4 sandbox";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./package.nix { };
      });

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.lua-sandbox;
          pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          options.services.lua-sandbox = {
            enable = lib.mkEnableOption "Lua 5.4 sandbox API";

            bind = lib.mkOption {
              type = lib.types.str;
              default = "127.0.0.1:8090";
              description = "Address the sandbox API listens on.";
            };
          };

          config = lib.mkIf cfg.enable {
            users.groups.lua-sandbox = { };
            users.users.lua-sandbox = {
              isSystemUser = true;
              group = "lua-sandbox";
            };

            systemd.services.lua-sandbox = {
              description = "Lua 5.4 sandbox API";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];
              environment = {
                LUA_WORKER_BIN = "${pkg}/bin/lua-worker";
                LUA_BIND = cfg.bind;
                LUA_SANDBOX_ROOT = "/var/lib/lua-sandbox";
              };
              serviceConfig = {
                ExecStart = "${pkg}/bin/lua-server";
                User = "lua-sandbox";
                Group = "lua-sandbox";
                StateDirectory = "lua-sandbox";
                Restart = "on-failure";
              };
            };
          };
        };
    };
}
