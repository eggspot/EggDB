#[test]
fn the_operator_refuses_to_start_outside_a_cluster() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_eggdb-operator"))
        .env("KUBERNETES_SERVICE_HOST", "")
        .output()
        .unwrap();
    if std::path::Path::new("/var/run/secrets/kubernetes.io/serviceaccount/token").exists() {
        return;
    }
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("service account"));
}
