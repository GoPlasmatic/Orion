<!-- description: The [channel_filter] settings: include and exclude glob patterns over channel names, for running separate fleets off one database. -->
<!-- type: reference -->
<!-- last_verified: 2026-09-14 -->

# Channel filter settings

The `[channel_filter]` section: which active channels this node loads, by glob pattern over the channel name.

## Synopsis

```toml
[channel_filter]
include = []
exclude = []
```

## Description

Both are matched against the channel name and are comma-separated in the env var. Use them to run separate fleets off one database without splitting the control plane. One instance serves `orders-*`, another serves the rest.

## Options

| Setting | Default | Env var | When to change |
|---|---|---|---|
| `channel_filter.include` | `[]` | `ORION_CHANNEL_FILTER__INCLUDE` | Glob patterns; empty loads every active channel. |
| `channel_filter.exclude` | `[]` | `ORION_CHANNEL_FILTER__EXCLUDE` | Applied after `include`. |

## Related

- [Deploy a cluster](../../operate/deploy/cluster.md): running fleets off one database.
- [Channels](../../concepts/channels.md): what a channel name identifies.
- [Engine settings](./engine.md): what happens at load.
- [Server configuration](./index.md): every section, by what you are configuring.
