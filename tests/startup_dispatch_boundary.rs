use std::fs;

#[test]
fn shared_auth_admission_precedes_early_command_dispatch() {
    let main = fs::read_to_string("src/main.rs").expect("reading src/main.rs");
    let prepare = main
        .find("zed_cli::cli_model::prepare_environment(&args);")
        .expect("startup must admit the embedded Shared Auth policy");
    let oci = main
        .find("oci_command::dispatch(args.clone())")
        .expect("OCI early dispatcher");

    assert!(
        prepare < oci,
        "Shared Auth policy admission must run before early command dispatch"
    );
    assert_eq!(
        main.matches("oci_command::dispatch(args.clone())").count(),
        1,
        "OCI commands must be dispatched exactly once during startup"
    );
}
