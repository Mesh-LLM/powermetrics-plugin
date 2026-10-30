# powermetrics-plugin

A [mesh-llm](https://github.com/Mesh-LLM/mesh-llm) plugin that measures how
much energy its node spends per output token, from the GPU's own power sensor,
and shows it in the node's `GET /api/status`:

```json
"plugins": {
  "powermetrics-plugin": {
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
      "watts": 212.7,
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
  while the snapshot is fresh, and `watts`, that last reading;
- `lost_events`: observation events the host dropped while this plugin read
  too slowly. The work running through a loss is left out of the rates.

The rates stay `null` until there is something to divide. The plugin publishes
every 5 s, and nothing at all until its first power reading, so a node without
a supported sensor has no `powermetrics-plugin` entry.

## Supported hardware

| GPU | Source | Notes |
| --- | --- | --- |
| NVIDIA, any OS | `nvidia-smi` | One reading per GPU each second, summed for the node. |
| Apple Silicon | IOReport through [macmon](https://github.com/vladkens/macmon), no sudo | The GPU's share of the SoC. |
| NVIDIA Jetson | none | The integrated GPU reports no board power, so no readings. |
| macOS VM | none | The GPU reads 0 W, which is treated as no source. |
| AMD, Windows | ADLX (`amdadlx64.dll`, installed by the Adrenalin driver), source `adlx` | Read only when `nvidia-smi` finds no GPU. Total board power where the driver reports it, else the GPU's own power. |
| AMD, Linux | amdgpu hwmon (`power1_average` or `power1_input`), source `amdgpu` | Read only when `nvidia-smi` finds no GPU. |
| AMD APU | the same, source `adlx-apu` or `amdgpu-apu` | Includes the CPU cores. Read only when no discrete AMD GPU reports power. |
| Intel | not yet | |

Board power covers the GPU only, not the CPU, memory or power supply, so the
figures are estimates, labelled with their source. An APU is the exception:
its CPU and GPU share one power figure, so its source says so.

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

## Install

```sh
mesh-llm plugins install Mesh-LLM/powermetrics-plugin
```

Release archives are built for macOS on Apple Silicon, Linux x86_64 and
ARM64, and Windows x86_64. The plugin needs a host with the
`local_inference_observation.v1` and `plugin_status.v1` capabilities
([mesh-llm#2608](https://github.com/Mesh-LLM/mesh-llm/pull/2608) and
[#2601](https://github.com/Mesh-LLM/mesh-llm/pull/2601)), and refuses to
initialize on an older one.

The observation feed is off unless the operator turns it on for this plugin,
so it needs, in the node's config:

```toml
[[plugin]]
name = "powermetrics-plugin"
allow_inference_observation = true
```

## Price floor

Give the plugin what you pay for electricity and it also reports what a
million output tokens cost you (mesh-llm #2320, step 2):

```toml
[[plugin]]
name = "powermetrics-plugin"
allow_inference_observation = true
args = ["--electricity-price-per-kwh", "0.25", "--currency", "EUR"]
```

`data` then carries `electricity_price_per_kwh`, `currency`, and two costs, in
the same currency:

- `cost_per_1m_output_tokens`: on top of the idle draw, the floor for a machine
  that runs anyway;
- `cost_per_1m_output_tokens_total`: idle draw included, the floor for a
  machine that runs only to serve.

They follow the energy rates, so they stay `null` until those are measured. The
figures cover the GPU's own sensor, not the rest of the machine, so they are a
lower bound. An unknown or malformed argument stops the plugin at startup.

Sellers price in msat per million tokens. Add the price of one bitcoin, in the
same currency as the electricity, and both costs are also given in msat:

```toml
args = ["--electricity-price-per-kwh", "0.25", "--currency", "EUR", "--btc-price", "95000"]
```

`data` then also carries `btc_price`, `cost_msat_per_1m_output_tokens` and
`cost_msat_per_1m_output_tokens_total`. The plugin asks no outside service for
the rate, so update it yourself when it moves.

## Energy page

The plugin adds an Energy page to the node's console. It shows the same
snapshot, refreshed every 5 s: the board power now, the idle draw, both
energy rates, the costs when a price is set, and the board power of the last
10 minutes against the idle draw. The page reads `/api/status` like any other
client and keeps nothing.

For each model the node serves, the page also sets the node's own output price
against its cost, from the `payment` block of `/v1/models` read every 30 s:

- **Above cost**: the price covers the cost with the idle draw included;
- **Pays only if the machine runs anyway**: above the cost on top of idle, below
  the cost with idle included;
- **Below cost**: every token loses money.

It shows the cheapest other seller of the same model next to it. The cost is
measured across every model the node serves, not per model, and the
comparison needs `--btc-price`; without it the prices show in msat only.

`plugin-manifest.json` declares the page to the installer and `bundle/energy.js`
draws it. The manifest is generated from the binary, and a test fails when the
two differ:

```sh
cargo run -- --print-package-manifest > plugin-manifest.json
```

## Build from source

```sh
cargo build --release
```

```toml
[[plugin]]
name = "powermetrics-plugin"
command = "/absolute/path/to/target/release/powermetrics-plugin"
allow_inference_observation = true
```

Releasing is described in [RELEASING.md](RELEASING.md).

## License

Apache-2.0
