{
  stdenv,
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "lua-sandbox";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./lua-protocol
      ./lua-host
      ./lua-worker
      ./lua-server
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  # Vendored Lua pulls in libgcc_s, which is not on the default loader path.
  buildInputs = [ stdenv.cc.cc.lib ];

  doCheck = false;

  # cargoInstallHook renames target/release to target/release-tmp before
  # postInstall runs, so install the workspace binaries from there.
  postInstall = ''
    install -D -m755 target/release-tmp/lua-worker "$out/bin/lua-worker"
    install -D -m755 target/release-tmp/lua-server "$out/bin/lua-server"
  '';

  meta = {
    description = "Internal HTTP API that runs Lua 5.4 in a sandbox";
    mainProgram = "lua-server";
  };
}
