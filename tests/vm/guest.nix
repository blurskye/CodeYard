# A disposable guest for tests/vm/hard-reset.sh; no host directories are mounted.
{ pkgs, codeyardBinary, webuiDist, sshPublicKey, database, sshPort, httpPort }:
let
  postgres = database == "postgres";
  codeyard = pkgs.runCommand "codeyard-vm-binary" {
    src = builtins.path { path = codeyardBinary; name = "codeyard"; };
    nativeBuildInputs = [ pkgs.autoPatchelfHook ];
    buildInputs = [ pkgs.libgit2 pkgs.stdenv.cc.cc.lib ];
  } ''
    install -Dm755 "$src" "$out/bin/codeyard"
  '';
in
(pkgs.nixos ({ lib, modulesPath, ... }: {
  imports = [ (modulesPath + "/virtualisation/qemu-vm.nix") ];
  system.stateVersion = "26.05";
  networking.hostName = "codeyard-durability";
  networking.firewall.allowedTCPPorts = [ 22 3000 ];
  boot.kernel.sysctl."kernel.sysrq" = 1;
  virtualisation = {
    memorySize = 4096;
    cores = 2;
    graphics = false;
    diskSize = 12288;
    useNixStoreImage = true;
    useHostCerts = false;
    sharedDirectories = lib.mkForce {};
    forwardPorts = [
      { host.address = "127.0.0.1"; host.port = sshPort; guest.port = 22; }
      { host.address = "127.0.0.1"; host.port = httpPort; guest.port = 3000; }
    ];
  };
  services.openssh = {
    enable = true;
    settings.PasswordAuthentication = false;
  };
  users.users.root.openssh.authorizedKeys.keys = [ sshPublicKey ];
  users.groups.codeyard = {};
  users.users.codeyard = { isSystemUser = true; group = "codeyard"; };
  environment.systemPackages = [ pkgs.git pkgs.curl pkgs.util-linux codeyard ];
  # Receive hooks check both executables; keep their parents root-owned and non-writable.
  system.activationScripts.codeyard-binary.text = ''
    install -Dm555 ${codeyard}/bin/codeyard /opt/codeyard/bin/codeyard
    install -Dm555 ${pkgs.git}/bin/git /opt/codeyard/bin/git
  '';
  services.postgresql = {
    enable = postgres;
    package = pkgs.postgresql_18;
    ensureDatabases = [ "codeyard" ];
    ensureUsers = [ { name = "codeyard"; ensureDBOwnership = true; } ];
  };
  systemd.services.codeyard = {
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ] ++ lib.optional postgres "postgresql.service";
    requires = lib.optional postgres "postgresql.service";
    path = [ pkgs.git pkgs.openssh ];
    environment = {
      PATH = lib.mkForce "/opt/codeyard/bin:${lib.makeBinPath [ pkgs.git pkgs.openssh pkgs.coreutils ]}";
      DATABASE_URL = if postgres then
        "postgresql://codeyard@localhost/codeyard?host=/run/postgresql" else
        "sqlite:///var/lib/codeyard/codeyard.db?mode=rwc";
      GITSERV_STATE_DIR = "/var/lib/codeyard/state";
      REPOS_PATH = "/var/lib/codeyard/repos";
      HOST_KEY_PATH = "/var/lib/codeyard/host_key";
      GITSERV_NODE_ROLES = "server";
      HTTP_ADDR = "0.0.0.0:3000";
      SSH_ADDR = "127.0.0.1:2222";
      SSH_MODE = "builtin";
      PUBLIC_URL = "http://127.0.0.1:${toString httpPort}";
      PUBLIC_HTTP_CLONE_BASE = "http://127.0.0.1:${toString httpPort}";
      COOKIE_SECURE = "false";
      WEBUI_DIST = builtins.path { path = webuiDist; name = "codeyard-webui"; };
      RUST_MIN_STACK = "33554432";
      RUST_LOG = "warn,codeyard=info";
    };
    preStart = ''
      if [ ! -e /var/lib/codeyard/secrets.env ]; then
        umask 077
        printf 'SECRETS_MASTER_KEY=' > /var/lib/codeyard/secrets.env
        ${pkgs.openssl}/bin/openssl rand -hex 32 >> /var/lib/codeyard/secrets.env
      fi
    '';
    serviceConfig = {
      User = "codeyard";
      Group = "codeyard";
      StateDirectory = "codeyard";
      WorkingDirectory = "/var/lib/codeyard";
      EnvironmentFile = "-/var/lib/codeyard/secrets.env";
      ExecStart = "/opt/codeyard/bin/codeyard serve";
      StandardOutput = "journal+console";
      StandardError = "journal+console";
      Restart = "on-failure";
    };
  };
})).config.system.build.vm
