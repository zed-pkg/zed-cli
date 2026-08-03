use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum OciCmd {
    /// Build and print the exact credential-free OCI publication plan
    Plan {
        /// Tagged destination, for example oci://ghcr.io/acme/tool:1.2.3
        destination: String,
        /// Select one language target from a polyglot package
        #[arg(long, env = "ZED_PKG_TARGET")]
        target: Option<String>,
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
                        json,
                    },
            } => {
                assert_eq!(destination, "oci://ghcr.io/acme/tool-rust:1.2.3");
                assert_eq!(target.as_deref(), Some("rust"));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
