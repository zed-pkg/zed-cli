{ pkgs }:

let
  bridge = pkgs.callPackage ../zed-package.nix { };

  # Evaluation tests must never execute the CLI. This derivation exists only so
  # fetchZedDeps receives a correctly shaped package while `nix flake check
  # --no-build` exercises its public argument and assertion contract.
  dummyZed = pkgs.writeShellScriptBin "zed-evaluation-only" ''
    echo "zed-pkg Nix evaluation contract unexpectedly executed the dummy CLI" >&2
    exit 99
  '';

  mkFetch = overrides:
    bridge.fetchZedDeps (
      {
        pname = "zed-nix-evaluation-contract";
        version = "0";
        src = ./.;
        zed = dummyZed;
        hash = pkgs.lib.fakeHash;
      }
      // overrides
    );

  evaluates = value: (builtins.tryEval value).success;

  defaultFetch = mkFetch { };
  explicitRegistryFetch = mkFetch {
    registry = "https://registry.example.invalid";
  };
  immutableRegistryFetch = mkFetch {
    registryPath = ./.;
  };
  routedFetch = mkFetch {
    adapter = "node";
    target = "nodejs";
  };

  consumer = bridge.mkZedPackage {
    pname = "zed-nix-evaluation-consumer";
    version = "0";
    src = ./.;
    zedDeps = defaultFetch;
    dontConfigure = true;
    dontBuild = true;
    passthru.marker = "caller-passthru-preserved";
    installPhase = ''
      mkdir -p "$out"
    '';
  };

  validDefault = evaluates defaultFetch.drvPath;
  validHttpsRegistry = evaluates explicitRegistryFetch.drvPath;
  validImmutableRegistry = evaluates immutableRegistryFetch.drvPath;
  validRouteMetadata = evaluates routedFetch.drvPath;
  validConsumer = evaluates consumer.drvPath;
  preservesCallerPassthru =
    consumer.passthru.marker == "caller-passthru-preserved";
  exposesVerifiedDeps =
    consumer.passthru.zedDeps.drvPath == defaultFetch.drvPath;

  rejectsAmbiguousRegistry = !(evaluates (mkFetch {
    registry = "https://registry.example.invalid";
    registryPath = ./.;
  }).drvPath);
  rejectsAdapterStorePath = !(evaluates (mkFetch {
    adapter = "node-/nix/store/secret";
  }).drvPath);
  rejectsTargetStorePath = !(evaluates (mkFetch {
    target = "nodejs-/nix/store/secret";
  }).drvPath);
in
assert validDefault;
assert validHttpsRegistry;
assert validImmutableRegistry;
assert validRouteMetadata;
assert validConsumer;
assert preservesCallerPassthru;
assert exposesVerifiedDeps;
assert rejectsAmbiguousRegistry;
assert rejectsAdapterStorePath;
assert rejectsTargetStorePath;
pkgs.runCommandNoCC "zed-nix-evaluation-contract" { } ''
  mkdir -p "$out"
  cat > "$out/result.txt" <<'EOF'
  zed-pkg Nix evaluation contract passed
  EOF
''
