# Loco Lokal contracts deployment

This repository contains the DocuSeal application. Its Helm chart is
`charts/loco-lokal-contracts`; the intended Application, release and Service name is
`loco-lokal-contracts`, in namespace `loco-lokal`. The proposed hostname is
`contracts.loco-lokal.de`. Review that hostname in `values.yaml` before publishing.

The preparation uses shared PostgreSQL and `local-retain` storage as agreed. It does not
change the running cluster, local Compose instance, DNS or Caddy. The app branch and the
accompanying infra patch must reach their respective `main` branches to deploy.

## ApplicationSet integration

The existing `loco-lokal` ApplicationSet scans `loco-lokal-internal`, with an explicit chart
allowlist. This repo stays separate. Apply `docs/loco-local-infra.patch` to
`loco-lokal/loco-local-infra` to add:

- An additional Git generator within the existing `loco-lokal` ApplicationSet, scanning only
  `charts/loco-lokal-contracts` in this repository.
- A `loco-lokal-contracts` ClusterSecretStore for its own Infisical project, scoped to
  namespace `loco-lokal`.
- A `contracts` database and login role on shared PostgreSQL, with password Secret
  `loco-lokal-postgres-contracts` sourced through that store.

The existing internal chart allowlist remains unchanged, including its exclusion of
`loco-lokal-social-posts`. Generator values carry each source repo and revision to the
Application template, using Argo CD's [Git generator values](https://argo-cd.readthedocs.io/en/stable/operator-manual/applicationset/Generators-Git/#pass-additional-key-value-pairs-via-values-field).
All existing Applications keep their names, namespace, chart paths, sources and sync policies.
Chart names must remain unique across repositories. There is no second contracts ApplicationSet.

The patch was prepared against infra commit
`e0e4c3967b531544e124ccebfe8e4b6e6ca64bd3`. Check it against current infra before applying:

```sh
# Run from a clean checkout of loco-local-infra. Use the actual path to this checkout.
git apply --check '/Users/vincent/Documents/ChatGPT/Loco Lokal Contracts/docs/loco-local-infra.patch'
git apply '/Users/vincent/Documents/ChatGPT/Loco Lokal Contracts/docs/loco-local-infra.patch'
```

The existing namespace already receives `ghcr-pull` and may use the `ghcr` store, so no new
pull-secret namespace registration is required. Argo CD's org credential must be able to read
this repo, and its registry token must be able to pull the contracts image.

## Secrets and database

Create an Infisical project with slug `loco-lokal-contracts` in organization `Loco-Lokal`.
Add the existing ESO machine identity as Viewer. In environment `prod`, folder `/`, set:

| Key | Purpose |
| --- | --- |
| `SECRET_KEY_BASE` | Persistent Rails key; also derives the default encryption key for stored settings and signing configuration. For a new instance generate a strong key privately. For migration, reuse the original value. |
| `CONTRACTS_DB_PASSWORD` | Password for database/role `contracts` on shared PostgreSQL. Use a strong random value. |

No secret values belong in Git or Helm values. The app's ExternalSecret copies
`SECRET_KEY_BASE` into `loco-lokal-contracts-env`; the infra database chart copies
`CONTRACTS_DB_PASSWORD` into the basic-auth Secret `loco-lokal-postgres-contracts`.
The Deployment reads the latter's `password` key as `DATABASE_PASSWORD`. Connection fields
are separate env vars, so passwords need no URL encoding. PostgreSQL host is
`loco-lokal-postgres-rw:5432`, with database and user `contracts`.

If an existing instance sets `ENCRYPTION_SECRET`, also copy that value to this Infisical
project and add `ENCRYPTION_SECRET` to the chart's `secretKeys` before migration. Preserve
the source `SECRET_KEY_BASE` even if it was auto-generated in `docuseal.env`; losing it can
make existing encrypted settings unreadable. Do not print these values into logs or chats.

Configure SMTP through DocuSeal's settings after initial setup. For environment-driven SMTP,
put non-secret settings under `env`, store `SMTP_USERNAME` and `SMTP_PASSWORD` in this
Infisical project, and add those names to `secretKeys`.

## Runtime and storage

CI already builds `ghcr.io/loco-lokal/loco-lokal-contracts:<full-commit-SHA>` and `:latest`
on every push to `main`, including chart-only changes, for amd64 and arm64. The ApplicationSet
keeps setting `image.tag=$ARGOCD_APP_REVISION`; no build workflow changes are needed.

Puma serves port 3000 and embeds Sidekiq and Redis. The Deployment runs one pod with
`Recreate` updates, giving it exclusive access to its data volume and serializing boot
migrations. Updates have downtime. The pod runs as the image's user/group 2000, and the
volume uses `fsGroup: 2000`. A 90-second termination grace period allows worker shutdown
and Redis's normal exit. Startup probes allow up to ten minutes for Rails boot/migrations.
Probes use `/up` and forwarded HTTPS; they check Rails boot, not document generation or
worker health. Memory starts at a 256 MiB request and 1 GiB limit; measure real PDF workloads.

The 10 GiB claim `loco-lokal-contracts-data` mounts at `/data/docuseal`, preserving documents,
Redis snapshots, and any local configuration. Argo CD `Prune=false,Delete=false` and Helm's keep annotation
protect the claim on removal; `local-retain` protects its underlying volume. Verify the
StorageClass has reclaim policy `Retain` and that minikube's provisioner directory is mounted
from the host before deployment. Retained data requires manual recovery after cluster
recreation. A released volume is not automatically reattached to a new claim.

Back up the data directory and PostgreSQL together, and preserve the Infisical keys. Redis
uses the application's existing snapshot persistence: a crash can lose jobs newer than the
last snapshot. Retained storage does not provide high availability, backups or crash-proof
queue persistence. Review a separate Redis/AOF setup if stronger queue durability is needed.

For an existing instance, stop writes and workers before the final database/files copy, and
restore into this target before starting the Kubernetes pod. The local Compose database is
PostgreSQL 18 while shared PostgreSQL is 16; verify dump/restore compatibility in a disposable
database first. Do not copy PostgreSQL data files across major versions or treat this chart
as a data migration. Preserve local volumes until the migrated app has been verified.

## Validate and publish

From this repository:

```sh
helm lint --strict charts/loco-lokal-contracts
helm template loco-lokal-contracts charts/loco-lokal-contracts -n loco-lokal \
  --set image.tag=0123456789abcdef0123456789abcdef01234567 \
  | kubectl -n loco-lokal apply --dry-run=server -f -
```

In the patched infra checkout, run `make lint` and server-side dry-runs for the affected charts
against the intended cluster. Its Argo CD, ESO and CloudNativePG CRDs must be installed:

```sh
helm template root charts/root -n argocd | kubectl -n argocd apply --dry-run=server -f -
helm template loco-lokal-postgres charts/loco-lokal-postgres -n loco-lokal \
  | kubectl -n loco-lokal apply --dry-run=server -f -
helm template cluster-secret-stores charts/cluster-secret-stores -n external-secrets \
  | kubectl -n external-secrets apply --dry-run=server -f -
kubectl get storageclass local-retain
```

Publish the app chart to this repo's `main` before publishing the infra change that scans it.
Prepare the Infisical project first. The Application may initially wait for its image, database
role or Secrets while CI, CloudNativePG and ESO reconcile. Do not manually Helm-install it;
Argo CD owns the deployment.

```sh
kubectl -n argocd get applicationset loco-lokal
kubectl -n argocd get application loco-lokal-contracts
kubectl get clustersecretstore loco-lokal-contracts
kubectl -n loco-lokal get database loco-lokal-postgres-contracts
kubectl -n loco-lokal get externalsecret loco-lokal-contracts-env loco-lokal-postgres-contracts
kubectl -n loco-lokal get pvc loco-lokal-contracts-data
kubectl -n loco-lokal rollout status deployment/loco-lokal-contracts --timeout=10m
kubectl -n loco-lokal get pod,service,ingress -l app.kubernetes.io/name=loco-lokal-contracts
```

Complete initial administrator setup over a private port-forward before public cutover:

```sh
kubectl -n loco-lokal port-forward service/loco-lokal-contracts 3000:80
```

Open `http://localhost:3000` for private setup. Production assumes a TLS-terminating proxy;
if the browser requires HTTPS for secure cookies, use a local TLS proxy for the port-forward
or an authenticated private ingress. Do not expose an unclaimed setup page publicly.
Check uploads, signing, generated PDF download, email delivery and a pod restart with the
same data before switching traffic.

## Public cutover

`contracts.loco-lokal.de` is a proposed hostname, not a verified DNS/Caddy configuration.
TLS terminates at Cloudflare/Caddy, so the chart's Traefik Ingress has no `tls` section.
On the server, first test the cluster route with the Host header:

```sh
curl -sS -H 'Host: contracts.loco-lokal.de' http://172.18.0.1:18080/up
docker service inspect caddy_caddy --format '{{index .Spec.Labels "caddy_10"}}'
```

Public cutover requires a separately reviewed Caddy update: preserve its current host list,
add the chosen hostname to the cluster route at `172.18.0.1:18080`, and remove any duplicate
Swarm hostname labels in the same operation. Persist the label in Caddy's stack configuration
too. Keep the old instance stopped with its data for rollback, and avoid running two sets of
workers against the same database. Confirm public `/up`, setup completion, signing and email.

## Preparation checks

Preparation passed these local checks with Helm 4.3.1:

- Strict lint and rendering of the app, root, PostgreSQL and secret-store charts.
- Comparison of 13 existing ApplicationSet chart patterns against unmodified infra, preserving
  their generated Application definitions; verification of the new source and per-source
  revision/path overrides.
- Cross-chart checks for database roles, password Secrets, store scope, Service routing,
  probes, persistent mounts and the full-SHA image workflow.
- Patch applicability and whitespace checks.

The local workspace has no Kubernetes context; the cluster host is reachable through SSH
alias `loco-lokal`, with Kubernetes access through `sudo -n kubectl`. Server-side dry-runs
passed for the app and affected infra charts against that cluster. Its root Application was
Synced/Healthy, its node Ready, and `local-retain` had reclaim policy Retain before publishing.
Rollout and image/runtime checks follow publishing. Existing-data migration and functional
signing tests require separate verification. Application source and the build workflow were
unchanged; application tests and a local container build were not run.
