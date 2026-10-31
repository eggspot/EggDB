#!/usr/bin/env bash
set -euo pipefail

project="${EGGDB_PROJECT:?set EGGDB_PROJECT to the Google Cloud project}"
region="${EGGDB_REGION:-us-central1}"
cluster="${EGGDB_CLUSTER:-eggdb}"
secret="${EGGDB_PASSWORD_SECRET:?set EGGDB_PASSWORD_SECRET to the Secret Manager secret holding the eggdb password}"
tag="${EGGDB_IMAGE_TAG:-latest}"
release="${EGGDB_RELEASE:-db}"
chart="$(cd "$(dirname "$0")/../helm/eggdb" && pwd)"

if ! gcloud container clusters describe "$cluster" --project "$project" --region "$region" > /dev/null 2>&1; then
  gcloud container clusters create-auto "$cluster" --project "$project" --region "$region"
fi
gcloud container clusters get-credentials "$cluster" --project "$project" --region "$region"
gcloud secrets versions access latest --project "$project" --secret "$secret" \
  | kubectl create secret generic "$release-password" --from-file=password=/dev/stdin --dry-run=client -o yaml \
  | kubectl apply -f -
helm upgrade --install "$release" "$chart" \
  --set existingSecret="$release-password" \
  --set image.tag="$tag" \
  --set storage.size=20Gi \
  --wait --timeout 10m
echo "EggDB runs as the $release StatefulSet in $cluster; reach it with: kubectl port-forward service/$release 5432:5432"
