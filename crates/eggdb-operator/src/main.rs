//! `eggdb-operator`: runs EggDB servers on Kubernetes for `EggDB` custom resources
//! (`eggspot.app/v1alpha1`). For each resource in its namespace it applies a Service and a
//! StatefulSet with one server on a persistent volume, owned by the resource so deleting the
//! resource deletes them, and reports the server's readiness in the resource's status. It reads
//! the cluster through the pod's service account; `deploy/operator` has the custom resource
//! definition, the role it needs and its Deployment.

mod json;
mod kube;
mod manifests;
mod reconcile;
mod spec;

use std::process::ExitCode;
use std::time::Duration;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);

fn interval() -> Duration {
    std::env::var("RECONCILE_SECONDS")
        .ok()
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_INTERVAL, Duration::from_secs)
}

fn main() -> ExitCode {
    let kubernetes = match kube::Kubernetes::in_cluster() {
        Ok(kubernetes) => kubernetes,
        Err(error) => {
            eprintln!("eggdb-operator: {error}");
            return ExitCode::from(2);
        }
    };
    let pause = interval();
    eprintln!(
        "eggdb-operator: watching EggDB resources in namespace {} every {}s",
        kubernetes.namespace,
        pause.as_secs()
    );
    let mut log = |line: &str| eprintln!("eggdb-operator: {line}");
    loop {
        if let Err(error) = reconcile::reconcile_all(&kubernetes, &mut log) {
            eprintln!("eggdb-operator: {error}");
        }
        std::thread::sleep(pause);
    }
}

#[cfg(test)]
mod tests {
    use crate::manifests::{service, stateful_set};
    use crate::spec::EggDbSpec;
    use eggdb_sql::json::parse_document;

    const RESOURCE: &str = r#"{"apiVersion": "eggspot.app/v1alpha1", "kind": "EggDB",
        "metadata": {"name": "orders", "namespace": "shop", "uid": "1234"},
        "spec": {"passwordSecret": {"name": "orders-password"}, "mysqlPort": 3306, "storage": {"size": "20Gi"}}}"#;

    #[test]
    fn a_resource_becomes_a_service_and_a_stateful_set_it_owns() {
        let resource = parse_document(RESOURCE).unwrap();
        let spec = EggDbSpec::read(&resource).unwrap();
        assert_eq!(spec.port, 5432);
        assert_eq!(spec.password_key, "password");
        let service = service(&spec);
        let set = stateful_set(&spec);
        assert!(parse_document(&service).is_ok(), "{service}");
        assert!(parse_document(&set).is_ok(), "{set}");
        assert!(set.contains("\"--tls-self-signed\""), "{set}");
        assert!(set.contains("\"0.0.0.0:3306\""), "{set}");
        assert!(set.contains("\"20Gi\""), "{set}");
        assert!(service.contains("\"uid\": \"1234\""), "{service}");
    }

    #[test]
    fn a_resource_without_a_password_secret_is_refused() {
        let resource = parse_document(r#"{"metadata": {"name": "x", "namespace": "y", "uid": "z"}, "spec": {}}"#).unwrap();
        assert!(EggDbSpec::read(&resource).is_err());
    }
}
