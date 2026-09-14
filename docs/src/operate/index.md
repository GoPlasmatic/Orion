<!-- description: Run Orion in production: deploy it, secure and observe it, and maintain it — promotion, backups, upgrades and troubleshooting — with the owning page for each. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Operate

These pages are for whoever runs Orion for other people. They assume you understand the [entity lifecycle](../concepts/lifecycle.md) and [packages](../concepts/packages.md), and each ends with the reference pages for the settings it names.

Start with the [production checklist](./production-checklist.md). Everything in Orion is off or permissive by default, because the defaults serve a laptop. The checklist links each go-live decision to the page that owns it.

Then the three groups. [Deploy Orion](./deploy/index.md) puts an instance, a Helm release or a cluster in place. [Secure and observe](./run/index.md) closes the surfaces you do not need and wires up the signals you do. [Maintain and recover](./maintain/index.md) is promotion, backups, upgrades and the symptom-indexed troubleshooting page.

| If you need to… | Start here |
|---|---|
| Deploy one instance | [Deploy with Docker](./deploy/docker.md) |
| Deploy on Kubernetes | [Deploy on Kubernetes](./deploy/kubernetes.md) |
| Run more than one replica | [Run a cluster](./deploy/cluster.md) |
| Secure the control and data planes | [Secure an instance](./run/security.md) |
| See what is happening | [Monitor and alert](./run/monitoring.md) and [Traces and async processing](./run/traces.md) |
| Bound what a failing dependency costs | [Timeouts, retries and circuit breakers](./run/failure-handling.md) |
| Move a service between environments | [Promote between environments](./maintain/promotion.md) |
| Recover, or move to a new version | [Back up and restore](./maintain/backup-restore.md) and [Upgrade an instance](./maintain/upgrades.md) |
| Diagnose a live problem | [Troubleshooting](./maintain/troubleshooting.md) |
