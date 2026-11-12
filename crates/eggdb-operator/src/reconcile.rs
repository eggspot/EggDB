use crate::json::{items, number, quoted};
use crate::kube::Kubernetes;
use crate::manifests::{service, stateful_set};
use crate::spec::EggDbSpec;

fn resources_path(namespace: &str) -> String {
    format!("/apis/eggspot.app/v1alpha1/namespaces/{namespace}/eggdbs")
}

/// Brings every `EggDB` resource of the namespace to what it asks for: its Service and
/// StatefulSet applied (server-side, so the operator owns only the fields it sets), and its
/// status set from the StatefulSet's ready replicas. Each resource's failure is reported in
/// its status and logged, and the others go on.
pub(crate) fn reconcile_all(kubernetes: &Kubernetes, log: &mut dyn FnMut(&str)) -> Result<usize, String> {
    let listed = kubernetes.get(&resources_path(&kubernetes.namespace))?;
    let mut reconciled = 0;
    for resource in items(&listed) {
        match EggDbSpec::read(resource) {
            Ok(spec) => match reconcile(kubernetes, &spec) {
                Ok(()) => reconciled += 1,
                Err(error) => {
                    log(&format!("{}: {error}", spec.name));
                    let status = format!("{{\"phase\": \"Failed\", \"message\": {}}}", quoted(&error));
                    let _ = kubernetes.merge_status(&format!("{}/{}", resources_path(&spec.namespace), spec.name), &status);
                }
            },
            Err(error) => log(&error),
        }
    }
    Ok(reconciled)
}

fn reconcile(kubernetes: &Kubernetes, spec: &EggDbSpec) -> Result<(), String> {
    let namespace = &spec.namespace;
    kubernetes.apply(&format!("/api/v1/namespaces/{namespace}/services/{}", spec.name), &service(spec))?;
    let set_path = format!("/apis/apps/v1/namespaces/{namespace}/statefulsets/{}", spec.name);
    kubernetes.apply(&set_path, &stateful_set(spec))?;
    let set = kubernetes.get(&set_path)?;
    let ready = number(&set, &["status", "readyReplicas"]).unwrap_or(0);
    let phase = if ready >= 1 { "Ready" } else { "Pending" };
    let endpoint = format!("{}.{namespace}.svc:{}", spec.name, spec.port);
    let status = format!(
        "{{\"phase\": {}, \"readyReplicas\": {ready}, \"endpoint\": {}, \"message\": \"\"}}",
        quoted(phase),
        quoted(&endpoint)
    );
    kubernetes.merge_status(&format!("{}/{}", resources_path(namespace), spec.name), &status)
}
