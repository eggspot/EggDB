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

Each release publishes the image for `linux/amd64` and `linux/arm64` as
`ghcr.io/eggspot/eggdb:<version>` and `:latest` (D224): `docker run -v eggdb:/data -v
$PWD/secrets:/secrets:ro -p 5432:5432 ghcr.io/eggspot/eggdb`, with the password in
`secrets/password`. The chart uses that image by default. To build one yourself, copy the binary
in and run `docker build -t eggdb:local deploy`. CI builds the image, installs the chart in a
kind cluster, runs SQL through it, restarts the pod and checks the data is still there.

Clouds, each a starting point to adapt (network, sizes, backups):
- `aws/eggdb.yaml`, a CloudFormation stack: one EC2 instance running the image with Docker, the
  database on an encrypted EBS volume that outlives the instance, the password read from a
  Secrets Manager secret, and PostgreSQL's port open only to `AllowedCidr`.
- `azure/main.bicep`: an Azure Container Instance running the image, the database on a premium
  Azure Files share and the password as a secret volume (`az deployment group create -f
  deploy/azure/main.bicep -p password=...`). Its public address takes connections from anywhere:
  put it in a virtual network for real use.
- `gcp/deploy.sh`: a GKE Autopilot cluster running this Helm chart, the password from Secret
  Manager (`EGGDB_PROJECT=... EGGDB_PASSWORD_SECRET=... deploy/gcp/deploy.sh`).

CI lints the three (cfn-lint, `az bicep build`, shellcheck). An operator and a Terraform
provider come with replication and clusters (D18).
