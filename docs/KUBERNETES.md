# Codeyard on Kubernetes

The chart at `deploy/helm/codeyard` runs web pods and a storage StatefulSet, with optional
SSH and trusted-workflow runner Deployments. See [cluster limits](CLUSTER.md#limits)
and the [readiness plan](CLUSTER_PLAN.md) for pending acceptance. The chart bundles
neither PostgreSQL nor an object store.

## Install

Use Kubernetes 1.27+, Helm 3, a persistent-volume StorageClass, external PostgreSQL and
S3-compatible storage. Publish the [Containerfile](../Containerfile) image to a reachable
registry; every role needs the same release, including role-aware storage `/ready` and
`/metrics`. Allow pod-to-pod TCP 7445; keep it and storage HTTP port 3000 private.

Create a Secret named `codeyard-config` in the installation namespace through your secret
manager. Required keys are `DATABASE_URL` (PostgreSQL, percent-encoded credentials),
`SECRETS_MASTER_KEY` (the installation's durable 64-hex key), `S3_ACCESS_KEY` and
`S3_SECRET_KEY`. Optional keys are `ADMIN_SECRET` and `METRICS_TOKEN`; the latter is
required for ServiceMonitor scrapes. Sessions live in PostgreSQL, so there is no separate
session-signing key or sticky-session requirement. Never replace an existing master key.

Save these non-secret settings as `codeyard-values.yaml`:

```yaml
image:
  repository: registry.example.net/codeyard
  tag: "your-release"
secrets:
  existingSecret: codeyard-config
config:
  publicUrl: https://git.example.com
  databasePoolSize: 7
  s3:
    endpoint: https://objects.example.net
    publicEndpoint: https://objects.example.net
    bucket: codeyard-files
    region: us-east-1
storage:
  persistence:
    size: 100Gi
ingress:
  enabled: true
  className: nginx
  host: git.example.com
  tlsSecretName: codeyard-tls
```

Install an ingress controller and provision that TLS Secret first. Codeyard speaks plain
HTTP inside the cluster. Set `config.httpTrustedProxyIps` to the controller's actual
source IPs (CIDRs are not accepted), and configure its Git upload-size and timeout limits. The public object
origin must be browser-reachable and outside the session cookie scope, not just another
port on the forge hostname. See [all values](../deploy/helm/codeyard/values.yaml).

```sh
helm upgrade --install codeyard deploy/helm/codeyard \
  --namespace forge --create-namespace -f codeyard-values.yaml
```

Finish administration at `/setup`. Startup migrates PostgreSQL under the application's
migration lock. Defaults run one web and one storage pod; optional features are off.

## Scale

- **Web:** set `web.replicas`, or enable `web.autoscaling.enabled` for the CPU HPA;
  metrics-server is required. Set minimum/maximum replicas and CPU requests to your budget.
- **SSH:** enable `ssh.enabled`; choose `ssh.service.type: LoadBalancer` or `NodePort`.
  Set `config.publicSshCloneBase` to the public address and port. Built-in SSH shares its
  host key through PostgreSQL; host OpenSSH is not supported by this chart.
- **Runner:** enable `runner.enabled` and set `runner.existingSecret` to a Secret containing
  a reusable **runner-only** `CLUSTER_JOIN_TOKEN`. Obtain it with
  `codeyard cluster invite --roles runner --reusable --expires 30d` on a reachable core.
  Set `runner.peers` to reachable web/SSH core key addresses, and `runner.replicas` for
  manual scaling. Dedicated hosts must already have a rootless Podman socket and writable
  workspace at `runner.podman.socketHostPath` and `runner.podman.workRoot`; the pod's uid/gid
  must have access. Both paths mount identically inside and outside the pod. This grants
  host-user execution, **not** isolation for hostile workflows; see [runner safety](CI_LOCAL_RUNNER.md).
- **KEDA:** install its controller/CRDs, enable `runner.keda.enabled`, and set the Prometheus
  address and query. Use `max(codeyard_ci_queued_runs{namespace="forge",service="codeyard-web"})`,
  adjusted to your release, rather than summing this installation-wide gauge across pods.
  Set the threshold to queued runs per runner and bound the replica count. Optional
  `runner.keda.authenticationRef` names an existing TriggerAuthentication; set `authModes`
  for your Prometheus authentication method.
- **Storage:** increase `storage.replicas`; each ordinal gets its own retained PVC and key.
  Core nodes normally self-register using PostgreSQL credentials. To require ticket-based
  enrollment, create a reusable **storage-only** invite on an existing reachable core and
  set `storage.join.existingSecret` to its Secret. The init container runs
  `codeyard cluster join` once per new PVC, without interactive input or manual `exec`.
  Joined pods reuse their saved identity; the ticket must not require manual approval.

For example, `kubectl -n forge scale statefulset/codeyard-storage --replicas=3` adds disks;
`kubectl -n forge scale deployment/codeyard-web --replicas=2` scales web when HPA is off.
Record manual counts in Helm values too, or the next upgrade restores the declared count.
Tickets expire and contain the issuer's key and IP addresses: rotate them before expiry
and after issuer replacement/address changes. Stateless web/SSH/runner pods get new node
identities when replaced; old memberships are not automatically removed.

With Prometheus Operator, enable `serviceMonitor.enabled` and match its labels to your
Prometheus selector. Scrapes use `METRICS_TOKEN`. Queue/disk/replica/node gauges are global;
pool size is per process and pool waiters reports `NaN`.

## Upgrade and limits

Back up PostgreSQL, repository PVCs and S3 separately before `helm upgrade` with the same
values and a new image tag/digest. Mixed releases are not supported: use a maintenance
window, stop workloads, upgrade all roles together, then restore replica counts. Storage
uses `OnDelete`; replace its pods deliberately, retaining PVCs. External Secret changes
need an explicit pod restart. Do not delete storage PVCs on uninstall.

Web/storage probes use `/ready`; SSH currently has TCP probes only and database-free
runners have no health listener/probes. SIGTERM performs the bounded server shutdown;
100 seconds is allowed; use the nodes page or drain API before removing pods. Disruption budgets do not
guarantee Git durability. Runner/KEDA scale-down can interrupt active jobs during
shutdown; it is not idle-only draining. Storage must **never**
autoscale down: first move and verify every primary and replica assignment elsewhere.
Provisioning, scaling policies and rebalancing are written but await verification.
Safe online mixed-version upgrades and cluster-wide backup/restore remain incomplete. Replication still has the [asynchronous loss window](CLUSTER.md#replicas).

`scripts/test-helm.sh` checks lint and default/all-feature/NodePort renders against strict
native schemas, skipping missing CRD schemas. It never contacts a Kubernetes cluster.

Use session pooling when placing PgBouncer in front of a complete cluster. Set
`config.databasePoolSize` to the per-process budget and allow additional sessions for
serving locks and notifications. See the [PgBouncer checks](CLUSTER.md#pgbouncer-check).
