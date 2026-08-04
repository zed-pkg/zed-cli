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
    /// Verify and copy one local OCI image layout to a registry through ORAS
    Push {
        /// OCI image-layout directory produced by `zed oci plan --out`
        layout: PathBuf,
        /// Tagged destination, for example oci://ghcr.io/acme/tool:1.2.3
        destination: String,
        /// ORAS executable path
        #[arg(long, env = "ZED_PKG_OCI_ORAS", default_value = "oras")]
        oras: PathBuf,
        /// Registry username; requires --password-stdin at runtime
        #[arg(long, env = "ZED_PKG_OCI_USERNAME")]
        username: Option<String>,
        /// Read one registry password or personal access token from stdin
        #[arg(long, env = "ZED_PKG_OCI_PASSWORD_STDIN")]
        password_stdin: bool,
        /// Explicit Docker/ORAS registry config; default credentials are never read implicitly
        #[arg(long, env = "ZED_PKG_OCI_REGISTRY_CONFIG")]
        registry_config: Option<PathBuf>,
        /// Push without registry credentials
        #[arg(long, env = "ZED_PKG_OCI_ANONYMOUS")]
        anonymous: bool,
        /// Use unencrypted HTTP; accepted only for loopback registries
        #[arg(long, env = "ZED_PKG_OCI_PLAIN_HTTP")]
        plain_http: bool,
        /// Skip destination TLS certificate verification
        #[arg(long, env = "ZED_PKG_OCI_INSECURE_TLS")]
        insecure_tls: bool,
        /// Custom destination registry CA certificate
        #[arg(long, env = "ZED_PKG_OCI_CA_FILE")]
        ca_file: Option<PathBuf>,
        /// Replace a remote tag only when its digest differs from the verified layout
        #[arg(long, env = "ZED_PKG_OCI_ALLOW_TAG_REPLACEMENT")]
        allow_tag_replacement: bool,
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_OCI_PUSH_JSON")]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::{CommandFactory, Parser};

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
                assert_eq!(out.as_deref(), Some(Path::new("dist/tool-layout")));
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn oci_push_has_typed_authentication_and_transport_flags() {
        let cli = Cli::try_parse_from([
            "zed",
            "oci",
            "push",
            "dist/tool-layout",
            "oci://127.0.0.1:5000/acme/tool:1.2.3",
            "--username",
            "acme-bot",
            "--password-stdin",
            "--plain-http",
            "--allow-tag-replacement",
            "--json",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Oci {
                cmd:
                    OciCmd::Push {
                        layout,
                        destination,
                        oras,
                        username,
                        password_stdin,
                        registry_config,
                        anonymous,
                        plain_http,
                        insecure_tls,
                        ca_file,
                        allow_tag_replacement,
                        json,
                    },
            } => {
                assert_eq!(layout, PathBuf::from("dist/tool-layout"));
                assert_eq!(
                    destination,
                    "oci://127.0.0.1:5000/acme/tool:1.2.3"
                );
                assert_eq!(oras, PathBuf::from("oras"));
                assert_eq!(username.as_deref(), Some("acme-bot"));
                assert!(password_stdin);
                assert!(registry_config.is_none());
                assert!(!anonymous);
                assert!(plain_http);
                assert!(!insecure_tls);
                assert!(ca_file.is_none());
                assert!(allow_tag_replacement);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn oci_json_environment_keys_are_command_scoped() {
        let root = Cli::command();
        let oci = root.find_subcommand("oci").unwrap();
        let plan = oci.find_subcommand("plan").unwrap();
        let push = oci.find_subcommand("push").unwrap();

        let plan_json = plan
            .get_arguments()
            .find(|argument| argument.get_long() == Some("json"))
            .and_then(|argument| argument.get_env())
            .unwrap()
            .to_string_lossy();
        let push_json = push
            .get_arguments()
            .find(|argument| argument.get_long() == Some("json"))
            .and_then(|argument| argument.get_env())
            .unwrap()
            .to_string_lossy();

        assert_eq!(plan_json, "ZED_PKG_OCI_JSON");
        assert_eq!(push_json, "ZED_PKG_OCI_PUSH_JSON");
    }

    #[test]
    fn oci_push_parser_defers_all_option_combinations_to_runtime() {
        for arguments in [
            vec![
                "zed",
                "oci",
                "push",
                "layout",
                "oci://ghcr.io/acme/tool:1.2.3",
            ],
            vec![
                "zed",
                "oci",
                "push",
                "layout",
                "oci://ghcr.io/acme/tool:1.2.3",
                "--anonymous",
                "--username",
                "acme-bot",
                "--password-stdin",
            ],
            vec![
                "zed",
                "oci",
                "push",
                "layout",
                "oci://ghcr.io/acme/tool:1.2.3",
                "--username",
                "acme-bot",
            ],
            vec![
                "zed",
                "oci",
                "push",
                "layout",
                "oci://127.0.0.1:5000/acme/tool:1.2.3",
                "--anonymous",
                "--plain-http",
                "--insecure-tls",
            ],
        ] {
            Cli::try_parse_from(arguments).unwrap();
        }
    }
}