#!/usr/bin/env bash
# Render-only checks: no image builds or Kubernetes access.
set -euo pipefail
cd "$(dirname "$0")/.."
chart=deploy/helm/codeyard
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

resource() {
    local file=$1 kind=$2 name=$3
    awk -v kind="$kind" -v name="$name" '
        BEGIN { RS="---"; ORS="" }
        index($0, "\nkind: " kind "\n") && index($0, "\n  name: " name "\n") { print }
    ' "$file"
}

helm lint "$chart"
helm lint "$chart" -f "$chart/ci/all-values.yaml"
helm template codeyard "$chart" --set config.databasePoolSize=7 > "$work/default.yaml"
helm template codeyard "$chart" --namespace forge -f "$chart/ci/all-values.yaml" > "$work/all.yaml"
helm template codeyard "$chart" --namespace forge -f "$chart/ci/all-values.yaml" \
    --set ssh.service.type=NodePort --set ssh.service.nodePort=30222 \
    --set serviceAccount.create=false --set serviceAccount.name=existing \
    --set-string storage.persistence.storageClass= > "$work/nodeport.yaml"
helm template codeyard "$chart" --namespace forge -f "$chart/ci/all-values.yaml" \
    --set postgres.cnpg.enabled=true --set ssh.service.type=ClusterIP > "$work/cnpg.yaml"
for file in default all nodeport cnpg; do
    kubeconform -strict -ignore-missing-schemas -summary < "$work/$file.yaml"
done

grep -q 'DATABASE_POOL_SIZE: "7"' "$work/default.yaml"

# The small install needs only web, repository storage and their configuration.
[[ -n $(resource "$work/default.yaml" Deployment codeyard-web) ]]
[[ -n $(resource "$work/default.yaml" StatefulSet codeyard-storage) ]]
for kind in Ingress HorizontalPodAutoscaler ScaledObject ServiceMonitor PodDisruptionBudget Cluster Pooler; do
    if grep -q "^kind: $kind$" "$work/default.yaml"; then
        echo "Unexpected $kind in the default install" >&2
        exit 1
    fi
done
[[ -z $(resource "$work/default.yaml" Deployment codeyard-runner) ]]
[[ -z $(resource "$work/default.yaml" Deployment codeyard-ssh) ]]
[[ -n $(resource "$work/default.yaml" Secret codeyard) ]]

# Every optional workload/controller is rendered, not silently ignored.
for kind in Ingress HorizontalPodAutoscaler ScaledObject ServiceMonitor; do
    grep -q "^kind: $kind$" "$work/all.yaml"
done
for role in web ssh storage runner; do
    [[ -n $(resource "$work/all.yaml" PodDisruptionBudget "codeyard-$role") ]]
done
resource "$work/all.yaml" Deployment codeyard-runner > "$work/runner.yaml"
grep -q 'name: CLUSTER_JOIN_TOKEN' "$work/runner.yaml"
if grep -Eq 'DATABASE_URL|SECRETS_MASTER_KEY|S3_SECRET_KEY|envFrom:' "$work/runner.yaml"; then
    echo 'Runner received core credentials' >&2
    exit 1
fi
resource "$work/all.yaml" StatefulSet codeyard-storage > "$work/storage.yaml"
grep -q 'initContainers:' "$work/storage.yaml"
grep -q 'timeout 60s codeyard cluster join' "$work/storage.yaml"
grep -q 'name: codeyard-storage-join' "$work/storage.yaml"
grep -q 'volumeClaimTemplates:' "$work/storage.yaml"
grep -q 'type: OnDelete' "$work/storage.yaml"
[[ -z $(resource "$work/all.yaml" Secret codeyard) ]]

# Exercise the other SSH exposure, existing account and explicit empty StorageClass.
resource "$work/nodeport.yaml" Service codeyard-ssh > "$work/ssh-service.yaml"
grep -q 'type: NodePort' "$work/ssh-service.yaml"
grep -q 'nodePort: 30222' "$work/ssh-service.yaml"
[[ -z $(resource "$work/nodeport.yaml" ServiceAccount codeyard) ]]
grep -q 'serviceAccountName: existing' "$work/nodeport.yaml"
grep -q 'storageClassName: ""' "$work/nodeport.yaml"
grep -q 'registry.example.net/codeyard@sha256:' "$work/nodeport.yaml"
# CloudNativePG: the chart owns the Cluster and Pooler and wires all three URLs from them.
[[ -n $(resource "$work/cnpg.yaml" Cluster codeyard-pg) ]]
[[ -n $(resource "$work/cnpg.yaml" Pooler codeyard-pgbouncer) ]]
resource "$work/cnpg.yaml" Cluster codeyard-pg | grep -q 'dataDurability: required'
resource "$work/cnpg.yaml" Pooler codeyard-pgbouncer | grep -q 'poolMode: transaction'
for role in web ssh; do
    resource "$work/cnpg.yaml" Deployment "codeyard-$role" > "$work/$role-cnpg.yaml"
    grep -q '@codeyard-pgbouncer:5432/codeyard?sslmode=verify-ca' "$work/$role-cnpg.yaml"
    grep -q '@codeyard-pg-ro:5432/codeyard?sslmode=verify-ca' "$work/$role-cnpg.yaml"
    grep -q '@codeyard-pg-rw:5432/codeyard?sslmode=verify-ca' "$work/$role-cnpg.yaml"
    grep -q 'secretName: codeyard-pg-ca' "$work/$role-cnpg.yaml"
    if grep -q 'key: DATABASE_URL' "$work/$role-cnpg.yaml"; then
        echo "CloudNativePG $role pods still read DATABASE_URL from the Secret" >&2
        exit 1
    fi
done
if grep -Eq 'CNPG_APP|DATABASE_|postgres-ca' <(resource "$work/cnpg.yaml" Deployment codeyard-runner); then
    echo 'Runner received database credentials' >&2
    exit 1
fi
if grep -q 'externalTrafficPolicy' <(resource "$work/cnpg.yaml" Service codeyard-ssh); then
    echo 'ClusterIP SSH Service has an external traffic policy' >&2
    exit 1
fi
echo 'Helm render checks passed (default, all features, NodePort, CloudNativePG and credential isolation).'
