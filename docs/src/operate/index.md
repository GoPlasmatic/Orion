<!-- description: Operate Orion in production: deployment, security, reliability, observability, promotion, backups, upgrades and troubleshooting. -->
# Operate Orion

**Page type:** Section guide · **Audience:** Developers and platform operators

Start with the [Production Checklist](./production-checklist.md). It links each
go-live decision to the page that owns the detailed procedure.

| Goal | Guide |
|---|---|
| Deploy one instance | [Docker](./docker.md) |
| Deploy on Kubernetes | [Kubernetes with Helm](./kubernetes.md) |
| Run multiple replicas | [Cluster Mode & High Availability](./cluster.md) |
| Secure control and data planes | [Secure an Instance](./security.md) |
| Observe health and performance | [Monitoring & Alerts](./monitoring.md) and [Traces](./traces.md) |
| Handle dependency failure | [Timeouts, Retries & Circuit Breakers](./failure-handling.md) |
| Move definitions between environments | [Promote Between Environments](./promotion.md) |
| Recover or upgrade | [Back Up & Restore](./backup-restore.md) and [Upgrades](./upgrades.md) |
| Diagnose a live problem | [Troubleshooting](./troubleshooting.md) |

Operational pages assume you already understand Orion's [entity
lifecycle](../concepts/lifecycle.md) and [packages](../concepts/packages.md).
