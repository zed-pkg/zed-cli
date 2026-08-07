#!/usr/bin/env python3
"""Executable DEN-1411 Zed ↔ Nix adapter oracle.

Implementation is split by boundary so export, sealing, and verification stay
reviewable while sharing one fail-closed validation core.
"""

from __future__ import annotations

from zed_nix_common import *  # noqa: F403
from zed_nix_export import command_zed_to_nix
from zed_nix_seal import command_nix_to_zed
from zed_nix_verify import command_verify


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="zed-nix-bridge")
    commands = parser.add_subparsers(dest="command", required=True)

    export = commands.add_parser("zed-to-nix", help="export one immutable Zed artifact")
    export.add_argument("--manifest", type=Path, default=Path(".zpkg.toml"))
    export.add_argument("--metadata", type=Path, required=True)
    export.add_argument("--nixpkgs-lock", type=Path, required=True)
    export.add_argument("--nixpkgs-url", default=DEFAULT_NIXPKGS_URL)
    export.add_argument("--system", action="append", default=[])
    export.add_argument("--out-dir", type=Path, required=True)
    export.add_argument("--allow-local-source", action="store_true")
    export.add_argument("--force", action="store_true")
    export.set_defaults(handler=command_zed_to_nix)

    seal = commands.add_parser("nix-to-zed", help="seal one closure-free realized Nix output")
    seal.add_argument("--store-path", type=Path, required=True)
    seal.add_argument("--path-info", type=Path, required=True)
    seal.add_argument("--derivation-json", type=Path, required=True)
    seal.add_argument("--flake-lock", type=Path, required=True)
    seal.add_argument("--locked-ref", required=True)
    seal.add_argument("--attribute", required=True)
    seal.add_argument("--system", required=True)
    seal.add_argument("--output", required=True)
    seal.add_argument("--as-package", required=True)
    seal.add_argument("--bin", action="append", default=[])
    seal.add_argument("--repository", required=True)
    seal.add_argument("--source-revision", required=True)
    seal.add_argument("--source-available", action="store_true")
    seal.add_argument("--license", required=True)
    seal.add_argument("--description", required=True)
    seal.add_argument("--nix-version", required=True)
    seal.add_argument("--out-dir", type=Path, required=True)
    seal.add_argument("--allow-local-store", action="store_true")
    seal.add_argument("--force", action="store_true")
    seal.set_defaults(handler=command_nix_to_zed)

    verify = commands.add_parser("verify", help="verify generated adapter evidence")
    verify.add_argument("--directory", type=Path, required=True)
    verify.set_defaults(handler=command_verify)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        args.handler(args)
    except BridgeError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except (OSError, json.JSONDecodeError, UnicodeDecodeError, tarfile.TarError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
