# NixOS QEMU VM for running benchmarks in isolation.
#
# The VM boots, runs the benchmark script as a systemd oneshot, writes
# results to /results (virtiofs-shared with the host), then powers off.
#
# All build tools are pre-populated in the read-only store image via
# additionalPaths.  Only compilation outputs are built inside the VM,
# on the writable overlay — these are what get timed.
#
# I/O setup:
#   - Nix store: 9p (cache=loose, msize=128KB) with kernel overlayfs
#   - Results:   9p (msize=128KB)
{ config, pkgs, lib, benchScript, additionalStorePaths, modulesPath, ... }:

{
  imports = [
    (modulesPath + "/virtualisation/qemu-vm.nix")
  ];

  # --- VM hardware ---
  virtualisation = {
    memorySize = 16384;
    cores = 4;
    diskSize = 30 * 1024; # 30 GB

    graphics = false;
    forwardPorts = [ ]; # no network needed

    # Ensure the store is writable so nix-store --realise can place outputs.
    writableStore = true;

    # Use disk-backed writable overlay (not tmpfs) so that registration
    # of output paths doesn't fill up memory with overlayfs copy-ups.
    writableStoreUseTmpfs = false;

    # Increase 9p msize from default 16KB to 128KB for better store
    # throughput.  The host store is shared via 9p with cache=loose.
    msize = 131072;

    # 9p shared directory for result collection.
    sharedDirectories.results = {
      source = "/tmp/cargo-schnee-bench-results";
      target = "/results";
    };
  };

  # Pre-populate the store with tool outputs and sources.
  # additionalPaths → closureInfo → regInfo: these paths (and their
  # closures) are registered in the VM's Nix DB at boot time.
  # They live on the read-only 9p-mounted host store and survive
  # nix-collect-garbage (which only cleans the writable overlay).
  virtualisation.additionalPaths = additionalStorePaths;

  # --- Nix daemon ---
  nix.settings = {
    experimental-features = [
      "ca-derivations"
      "recursive-nix"
      "dynamic-derivations"
      "flakes"
      "nix-command"
    ];
    system-features = [ "recursive-nix" "benchmark" "big-parallel" ];

    # No substituters — everything must be in the local store or built locally.
    substituters = lib.mkForce [ ];

    # Disable sandbox — builds need direct access to paths on the
    # 9p-backed overlayfs store.  Sandboxed builds fail with "build
    # input does not exist" because bind-mounts from 9p don't work
    # inside the chroot.
    sandbox = false;
    max-jobs = 4;
    cores = 4;

    # Fsync store paths before registering them in the DB so interrupted
    # builds never leave empty/truncated files in the overlay upper layer.
    sync-before-registering = true;
  };

  # The daemon uses the default store at /nix/store (the kernel overlayfs
  # merged view).  local-overlay-store was removed because it is
  # incompatible with recursive-nix + CA derivations (causes SIGKILL on
  # build scripts).  The overlayfs is transparent to the daemon — writes
  # go to the upper layer automatically via the kernel.

  # --- Register build-time paths ---
  # Build-time tool outputs, .drv files, and source paths (builder.sh,
  # setup scripts) physically exist on the 9p store but aren't in the
  # runtime closure captured by additionalPaths/closureInfo.  The host
  # runner generates --load-db registration data with 0 references
  # (avoids reference validation issues with not-yet-built outputs).
  # This MUST run before nix-daemon starts so the daemon sees the
  # registered paths when it opens its SQLite connection.
  fileSystems."/results".neededForBoot = true;
  boot.postBootCommands = lib.mkAfter ''
    echo "=== store-reg: checking /results/store-reg.txt ==="
    if [ -f /results/store-reg.txt ]; then
      LINES=$(wc -l < /results/store-reg.txt)
      echo "=== store-reg: found $LINES lines, loading... ==="
      ${config.nix.package.out}/bin/nix-store --load-db < /results/store-reg.txt
      echo "=== store-reg: load-db exit code: $? ==="
    else
      echo "=== store-reg: FILE NOT FOUND ==="
      ls -la /results/ 2>&1 || echo "=== /results not mounted ==="
    fi

    # The VM's Nix DB starts fresh (on the writable disk) but the
    # nix-store --load-db above may have indirectly registered paths.
    # Scrub compilation artifacts from the DB so every build system
    # starts from scratch.
    echo "=== Cleaning compilation artifacts from Nix DB ==="
    DB="/nix/var/nix/db/db.sqlite"
    SQLITE="${pkgs.sqlite}/bin/sqlite3"
    if [ -f "$DB" ]; then
      echo "  ValidPaths total: $($SQLITE "$DB" 'SELECT COUNT(*) FROM ValidPaths;')"
      echo "  Crate compiled:   $($SQLITE "$DB" "SELECT COUNT(*) FROM ValidPaths WHERE path LIKE '%/%-crate-%' AND path NOT LIKE '%.drv' AND path NOT LIKE '%tar.gz';")"
      echo "  Realisations:     $($SQLITE "$DB" 'SELECT COUNT(*) FROM Realisations;')"

      # Remove Realisations pointing to compilation outputs, then Refs,
      # then ValidPaths.  Also nuke ALL Realisations as a safety net —
      # the VM must build everything from scratch.
      $SQLITE "$DB" <<'SQL'
        DELETE FROM Realisations;
        DELETE FROM Refs WHERE reference IN (
          SELECT id FROM ValidPaths
          WHERE (path LIKE '%/%-crate-%' AND path NOT LIKE '%.drv' AND path NOT LIKE '%tar.gz')
             OR ((path LIKE '%/%-just-1.%' OR path LIKE '%/%-just-deps-%') AND path NOT LIKE '%.drv')
        );
        DELETE FROM Refs WHERE referrer IN (
          SELECT id FROM ValidPaths
          WHERE (path LIKE '%/%-crate-%' AND path NOT LIKE '%.drv' AND path NOT LIKE '%tar.gz')
             OR ((path LIKE '%/%-just-1.%' OR path LIKE '%/%-just-deps-%') AND path NOT LIKE '%.drv')
        );
        DELETE FROM ValidPaths
        WHERE (path LIKE '%/%-crate-%' AND path NOT LIKE '%.drv' AND path NOT LIKE '%tar.gz')
           OR ((path LIKE '%/%-just-1.%' OR path LIKE '%/%-just-deps-%') AND path NOT LIKE '%.drv');
SQL

      echo "  After cleanup:"
      echo "    ValidPaths:    $($SQLITE "$DB" 'SELECT COUNT(*) FROM ValidPaths;')"
      echo "    Crate compiled: $($SQLITE "$DB" "SELECT COUNT(*) FROM ValidPaths WHERE path LIKE '%/%-crate-%' AND path NOT LIKE '%.drv' AND path NOT LIKE '%tar.gz';")"
      echo "    Realisations:   $($SQLITE "$DB" 'SELECT COUNT(*) FROM Realisations;')"
    fi
  '';

  # --- Benchmark service ---
  systemd.services.benchmark = {
    description = "cargo-schnee benchmark suite";
    wantedBy = [ "multi-user.target" ];
    after = [ "nix-daemon.service" ];
    requires = [ "nix-daemon.service" ];

    # Give the benchmark plenty of time (2 hours max).
    serviceConfig = {
      Type = "oneshot";
      ExecStart = "${benchScript}";
      TimeoutStartSec = "7200";
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
  };

  # --- System packages available in the VM ---
  environment.systemPackages = with pkgs; [
    jq
    time
    sqlite  # Used in postBootCommands for DB cleanup
  ];

  # --- Minimal system config ---
  system.stateVersion = "24.11";
  users.users.root.initialPassword = "root";

  # Speed up boot — skip unnecessary services.
  documentation.enable = false;
  services.udisks2.enable = false;
  networking.firewall.enable = false;
}
