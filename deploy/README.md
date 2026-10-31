# Deploying EggDB

- `Dockerfile` builds an image from a static `eggdb` binary (the release's
  `x86_64-unknown-linux-musl` build, copied next to it as `eggdb`): the binary alone, run as an
  unprivileged user, serving `/data/eggdb.eggdb` over PostgreSQL's protocol with TLS on port
  5432 and Prometheus metrics on port 9187, with the password from `/secrets/password`.
- `kubernetes/eggdb.yaml` runs that image as a one-replica StatefulSet with a volume, a Secret
  for the password (change it) and a Service.
- `helm/eggdb` is the same as a Helm chart: `helm install db deploy/helm/eggdb --set
  password=...` (or `existingSecret`), with the image, storage size and class, resources and
  extra `eggdb serve` arguments as values.

No image is published yet; build it with `docker build -t eggdb:local deploy` after copying the
binary in. CI builds the image, installs the chart in a kind cluster, runs SQL through it,
restarts the pod and checks the data is still there. An operator and a Terraform provider come
with replication and clusters (D18).
