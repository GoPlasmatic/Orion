<!-- description: Run Orion in production: secure the control and data planes, monitor and alert, read traces, handle dependency failure, and keep an audit trail. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Secure and observe

Five pages for the instance that is now taking traffic you did not send. The first closes what should be closed; the other four are the signals and the controls you run it by.

- [Secure an instance](./security.md) turns on admin auth, decides how each channel authenticates, terminates TLS, trusts the right proxies, and keeps credentials out of the database.
- [Monitor and alert](./monitoring.md) wires up structured logs, Prometheus metrics, OpenTelemetry spans and the three health endpoints, and names the seven signals worth alerting on.
- [Traces and async processing](./traces.md) chooses a trace-storage mode, runs an async channel, drains the dead-letter queue, and keeps the traces table bounded.
- [Timeouts, retries and circuit breakers](./failure-handling.md) bounds what a failing dependency costs, and walks the shutdown sequence a rolling deploy depends on.
- [Audit logs](./audit-logs.md) reads the trail of every admin mutation, groups a multi-step operation under one change context, and bounds retention.
