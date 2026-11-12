use crate::json::quoted;
use crate::spec::EggDbSpec;

pub(crate) const API_VERSION: &str = "eggspot.app/v1alpha1";
pub(crate) const KIND: &str = "EggDB";
const DATA_PATH: &str = "/data";

fn owner(spec: &EggDbSpec) -> String {
    format!(
        "[{{\"apiVersion\": {}, \"kind\": {}, \"name\": {}, \"uid\": {}, \"controller\": true, \"blockOwnerDeletion\": true}}]",
        quoted(API_VERSION),
        quoted(KIND),
        quoted(&spec.name),
        quoted(&spec.uid)
    )
}

fn labels(spec: &EggDbSpec) -> String {
    format!(
        "{{\"app.kubernetes.io/name\": \"eggdb\", \"app.kubernetes.io/instance\": {}, \"app.kubernetes.io/managed-by\": \"eggdb-operator\"}}",
        quoted(&spec.name)
    )
}

fn ports(spec: &EggDbSpec, container: bool) -> String {
    let mut ports = vec![("postgresql", spec.port)];
    if let Some(port) = spec.mysql_port {
        ports.push(("mysql", port));
    }
    let entries: Vec<String> = ports
        .iter()
        .map(|(name, port)| {
            if container {
                format!("{{\"name\": {}, \"containerPort\": {port}}}", quoted(name))
            } else {
                format!("{{\"name\": {}, \"port\": {port}, \"targetPort\": {}}}", quoted(name), quoted(name))
            }
        })
        .collect();
    format!("[{}]", entries.join(", "))
}

/// The Service in front of the server, named after the resource.
pub(crate) fn service(spec: &EggDbSpec) -> String {
    format!(
        "{{\"apiVersion\": \"v1\", \"kind\": \"Service\", \"metadata\": {{\"name\": {}, \"namespace\": {}, \"labels\": {}, \"ownerReferences\": {}}}, \"spec\": {{\"selector\": {}, \"ports\": {}}}}}",
        quoted(&spec.name),
        quoted(&spec.namespace),
        labels(spec),
        owner(spec),
        labels(spec),
        ports(spec, false)
    )
}

fn server_arguments(spec: &EggDbSpec) -> Vec<String> {
    let mut arguments = vec![
        "serve".to_owned(),
        format!("{DATA_PATH}/eggdb"),
        "--create".to_owned(),
        "--listen".to_owned(),
        format!("0.0.0.0:{}", spec.port),
    ];
    if let Some(port) = spec.mysql_port {
        arguments.extend(["--mysql-listen".to_owned(), format!("0.0.0.0:{port}")]);
    }
    if spec.tls_secret.is_some() {
        arguments.extend([
            "--tls-certificate".to_owned(),
            "/tls/tls.crt".to_owned(),
            "--tls-key".to_owned(),
            "/tls/tls.key".to_owned(),
        ]);
    } else {
        arguments.push("--tls-self-signed".to_owned());
    }
    arguments
}

fn resources(spec: &EggDbSpec) -> String {
    let mut requests = Vec::new();
    if let Some(cpu) = &spec.cpu {
        requests.push(format!("\"cpu\": {}", quoted(cpu)));
    }
    if let Some(memory) = &spec.memory {
        requests.push(format!("\"memory\": {}", quoted(memory)));
    }
    if requests.is_empty() {
        return "{}".to_owned();
    }
    let requested = requests.join(", ");
    format!("{{\"requests\": {{{requested}}}, \"limits\": {{{requested}}}}}")
}

/// The StatefulSet that runs one server on a persistent volume, named after the resource.
pub(crate) fn stateful_set(spec: &EggDbSpec) -> String {
    let arguments: Vec<String> = server_arguments(spec).iter().map(|argument| quoted(argument)).collect();
    let mut mounts = vec![format!("{{\"name\": \"data\", \"mountPath\": {}}}", quoted(DATA_PATH))];
    let mut volumes = Vec::new();
    if let Some(secret) = &spec.tls_secret {
        mounts.push("{\"name\": \"tls\", \"mountPath\": \"/tls\", \"readOnly\": true}".to_owned());
        volumes.push(format!("{{\"name\": \"tls\", \"secret\": {{\"secretName\": {}}}}}", quoted(secret)));
    }
    let storage_class = spec
        .storage_class
        .as_ref()
        .map(|class| format!(", \"storageClassName\": {}", quoted(class)))
        .unwrap_or_default();
    format!(
        "{{\"apiVersion\": \"apps/v1\", \"kind\": \"StatefulSet\", \
         \"metadata\": {{\"name\": {name}, \"namespace\": {namespace}, \"labels\": {labels}, \"ownerReferences\": {owner}}}, \
         \"spec\": {{\"serviceName\": {name}, \"replicas\": 1, \"selector\": {{\"matchLabels\": {labels}}}, \
         \"template\": {{\"metadata\": {{\"labels\": {labels}}}, \"spec\": {{\
         \"containers\": [{{\"name\": \"eggdb\", \"image\": {image}, \"args\": [{arguments}], \"ports\": {ports}, \
         \"env\": [{{\"name\": \"EGGDB_PASSWORD\", \"valueFrom\": {{\"secretKeyRef\": {{\"name\": {secret}, \"key\": {key}}}}}}}], \
         \"readinessProbe\": {{\"tcpSocket\": {{\"port\": \"postgresql\"}}, \"periodSeconds\": 5}}, \
         \"resources\": {resources}, \"volumeMounts\": [{mounts}]}}], \"volumes\": [{volumes}]}}}}, \
         \"volumeClaimTemplates\": [{{\"metadata\": {{\"name\": \"data\"}}, \"spec\": {{\"accessModes\": [\"ReadWriteOnce\"], \
         \"resources\": {{\"requests\": {{\"storage\": {storage}}}}}{storage_class}}}}}]}}}}",
        name = quoted(&spec.name),
        namespace = quoted(&spec.namespace),
        labels = labels(spec),
        owner = owner(spec),
        image = quoted(&spec.image),
        arguments = arguments.join(", "),
        ports = ports(spec, true),
        secret = quoted(&spec.password_secret),
        key = quoted(&spec.password_key),
        resources = resources(spec),
        mounts = mounts.join(", "),
        volumes = volumes.join(", "),
        storage = quoted(&spec.storage),
    )
}
