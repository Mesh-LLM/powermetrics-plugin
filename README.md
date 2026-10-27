# powermetrics

A [mesh-llm](https://github.com/Mesh-LLM/mesh-llm) plugin that measures how
much energy its node spends per output token, from the GPU's own power sensor,
and shows it in the node's `GET /api/status`:

```json
"plugins": {
  "powermetrics": {
    "schema_version": 1,
    "state": "ready",
    "received_at_unix_ms": 1793404800000,
    "fresh_until_unix_ms": 1793404815000,
    "data": {
      "source": "nvidia-smi",
      "idle_watts": 18.4,
      "wh_per_1k_output_tokens": 0.62,
      "wh_per_1k_output_tokens_total": 0.71,
      "output_tokens": 48213,
      "busy_seconds": 912.0,
      "idle_seconds": 3120.0,
      "last_reading_unix_ns": 1793404799512000000,
      "lost_events": 0
    }
  }
}
```

The figures above are an example. The envelope is the host's; `data` is this
plugin's:

- `idle_watts`: the node's GPU draw while nothing runs, once 5 s of idle time
  are measured;
- `wh_per_1k_output_tokens`: the energy each 1,000 output tokens add on top of
  that idle draw, and `wh_per_1k_output_tokens_total` including it;
- `output_tokens`, `busy_seconds`, `idle_seconds`: what the rates are made of;
- `source` and `last_reading_unix_ns`, so a sensor that went quiet shows even
  while the snapshot is fresh;
- `lost_events`: observation events the host dropped while this plugin read
  too slowly. The work running through a loss is left out of the rates.

The rates stay `null` until there is something to divide. The plugin publishes
every 5 s, and nothing at all until its first power reading, so a node without
a supported sensor has no `powermetrics` entry.

## Supported hardware

| GPU | Source | Notes |
| --- | --- | --- |
| NVIDIA, any OS | `nvidia-smi` | One reading per GPU each second, summed for the node. |
| Apple Silicon | IOReport through [macmon](https://github.com/vladkens/macmon), no sudo | The GPU's share of the SoC. |
| NVIDIA Jetson | none | The integrated GPU reports no board power, so no readings. |
| macOS VM | none | The GPU reads 0 W, which is treated as no source. |
| AMD, Intel | not yet | |

Board power covers the GPU only, not the CPU, memory or power supply, so the
figures are estimates, labelled with their source.

## How it measures

What runs comes from the host's local inference observation feed: executions
of this node's own models, including requests from peers, never work it only
forwards. Each interval between two readings is busy when an execution ran in
any part of it, and idle otherwise; concurrent executions share a busy
interval's energy. Readings start counting once a local model is served, so the
idle draw is the node's with a model loaded.

An execution's tokens only count with its share of the busy energy, and only
when its whole run was measured: a sampling gap longer than 10 s, a reading
missing at its start or end, or a loss in the feed leaves it out. Readings and
events are applied in time order, and an interval is settled only once the
feed's watermark has passed it, so a late event still lands in the interval it
belongs to.

The host has no energy-specific code. This is the meter from
[mesh-llm#2518](https://github.com/Mesh-LLM/mesh-llm/pull/2518), moved into a
plugin as [mesh-llm#2581](https://github.com/Mesh-LLM/mesh-llm/issues/2581)
asked. It is the first step of
[mesh-llm#2320](https://github.com/Mesh-LLM/mesh-llm/issues/2320): a seller can
only price a GPU against its electricity once the energy per token is known.

## Build and run

It needs a host with the `local_inference_observation.v1` and
`plugin_status.v1` capabilities
([mesh-llm#2608](https://github.com/Mesh-LLM/mesh-llm/pull/2608) and
[#2601](https://github.com/Mesh-LLM/mesh-llm/pull/2601)), and refuses to
initialize on an older one.

```sh
cargo build --release
```

```toml
[[plugin]]
name = "powermetrics"
command = "/absolute/path/to/target/release/powermetrics"
allow_inference_observation = true
```

The observation feed is off unless the operator turns it on for this plugin,
so `allow_inference_observation = true` is required.

## License

Apache-2.0
