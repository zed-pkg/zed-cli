use std::path::PathBuf;

use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum OciCmd {
    /// Build the exact credential-free OCI publication plan, optionally materializing a layout
    Plan {
        /// Tagged destination, for example oci://ghcr.io/acme/tool:1.2.3
        destination: String,
        /// Select one language target from a polyglot package
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
        /// Materialize a standard OCI image-layout directory at this path
        #[arg(long, env = "ZED_PKG_PACK_OUT")]
        out: Option<PathBuf>,
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_OCI_JSON")]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{Cli, Cmd};

    use super::OciCmd;

    #[test]
    fn oci_plan_has_typed_destination_target_and_json_flags() {
        let cli = Cli::try_parse_from([
            "zed",
            "oci",
            "plan",
            "oci://ghcr.io/acme/tool-rust:1.2.3",
            "--target",
            "rust",
            "--json",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Oci {
                cmd:
                    OciCmd::Plan {
                        destination,
                        target,
                        out,
                        json,
                    },
            } => {
                assert_eq!(destination, "oci://ghcr.io/acme/tool-rust:1.2.3");
                assert_eq!(target.as_deref(), Some("rust"));
                assert!(out.is_none());
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn oci_plan_accepts_an_explicit_layout_output() {
        let cli = Cli::try_parse_from([
            "zed",
            "oci",
            "plan",
            "oci://ghcr.io/acme/tool:1.2.3",
            "--out",
            "dist/tool-layout",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Oci {
                cmd:
                    OciCmd::Plan {
                        destination,
                        target,
                        out,
                        json,
                    },
            } => {
                assert_eq!(destination, "oci://ghcr.io/acme/tool:1.2.3");
                assert!(target.is_none());
                assert_eq!(out.as_deref(), Some(std::path::Path::new("dist/tool-layout")));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
