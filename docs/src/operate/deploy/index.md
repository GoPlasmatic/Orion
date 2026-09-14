<!-- description: Deploy Orion for production: a single instance with Docker, a Helm release on Kubernetes, or a multi-node cluster sharing one database and one Redis. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Deploy Orion

One binary, three shapes. Each page here is one of them, and the third explains what the other two are configuring.

- [Deploy with Docker](./docker.md) runs the image with a durable volume, a drain budget the orchestrator respects, and health probes. It also holds the reference HA compose topology.
- [Deploy on Kubernetes](./kubernetes.md) installs the official Helm chart: N replicas in cluster mode, a pre-upgrade migration Job, surge rolling deploys, and a dedicated metrics listener.
- [Run a cluster](./cluster.md) says what replicas share, how a change reaches every node, what stays per node, and why migrations are a deploy step rather than a boot step.
