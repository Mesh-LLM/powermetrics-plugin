//! powermetrics-plugin: a mesh-llm plugin that measures the energy its node spends
//! serving and shows it in `GET /api/status` under `plugins.powermetrics-plugin`.
//!
//! It reads GPU power (`nvidia-smi`, or macmon on Apple Silicon) and follows
//! what the node executes through the local inference observation feed, then
//! reports the idle draw and the energy per 1,000 output tokens on top of it.
//! The host has no energy-specific code: this is mesh-llm #2518's meter, moved
//! out of the host as #2581 asked.
//!
//! ```toml
//! [[plugin]]
//! name = "powermetrics-plugin"
//! command = "/path/to/powermetrics-plugin"
//! allow_inference_observation = true
//! ```
//!
//! With an electricity price in its `args`, it also reports what a million
//! output tokens cost (see `price`). Its `bundle/` adds an Energy page to the
//! console that shows the same snapshot.

mod meter;
mod price;
mod sampler;
mod settle;

use anyhow::Context;
use mesh_llm_plugin::inference_observation::{InferenceObservationItem, inference_observation};
use mesh_llm_plugin::plugin_status::plugin_status;
use mesh_llm_plugin::{
    Plugin, PluginContext, PluginMetadata, PluginRuntime, SimplePlugin, package_manifest_json,
    plugin_manifest, plugin_server_info, web_ui, web_ui_bundle, web_ui_page,
};
use serde::Serialize;
use std::time::Duration;
use tokio::sync::mpsc;

const NAME: &str = "powermetrics-plugin";
const PUBLISH_EVERY: Duration = Duration::from_secs(5);

/// Version 1 of this plugin's `/api/status` snapshot.
#[derive(Serialize)]
struct Snapshot {
    /// When the last reading was taken, Unix nanoseconds: a sensor that went
    /// quiet shows here, whatever the host's freshness says.
    last_reading_unix_ns: Option<u64>,
    /// That last reading: board power summed over the GPUs, in watts.
    watts: Option<f64>,
    /// Events the host dropped while this plugin read too slowly; the work
    /// running through a loss is left out of the rates.
    lost_events: u64,
    #[serde(flatten)]
    energy: meter::EnergyStatusPayload,
    #[serde(flatten)]
    cost: Option<price::CostPayload>,
}

async fn run(mut context: PluginContext<'static>, tariff: Option<price::Tariff>) {
    let (readings_tx, mut readings) = mpsc::unbounded_channel();
    sampler::start(readings_tx);
    let mut feed = match context.subscribe_inference_observation().await {
        Ok(feed) => feed,
        Err(error) => {
            eprintln!("{NAME}: no inference observation, so no energy: {error}");
            return;
        }
    };
    let mut settler = settle::Settler::default();
    settler.start_from(feed.snapshot());
    let mut source = None;
    let mut last_reading = None;
    let mut last_watts = None;
    let mut lost_events = 0;
    let mut publish = tokio::time::interval(PUBLISH_EVERY);
    loop {
        tokio::select! {
            item = feed.next() => match item {
                Some(InferenceObservationItem::Batch(batch)) => {
                    lost_events += batch.loss.as_ref().map_or(0, |loss| loss.lost_events);
                    settler.batch(batch);
                }
                Some(InferenceObservationItem::Overflowed) => {
                    // This plugin dropped batches: start again from a snapshot,
                    // leaving what ran meanwhile out of the rates.
                    settler.meter.lose_track_of_running();
                    feed = match context.subscribe_inference_observation().await {
                        Ok(feed) => feed,
                        Err(error) => {
                            eprintln!("{NAME}: lost the inference observation feed: {error}");
                            return;
                        }
                    };
                    settler.start_from(feed.snapshot());
                }
                None => return,
            },
            Some((name, (at, watts))) = readings.recv() => {
                source = Some(name);
                last_reading = Some(at);
                last_watts = Some(watts);
                settler.reading(at, watts);
            }
            _ = publish.tick() => {
                let Some(source) = source else { continue };
                let energy = settler.meter.status(source);
                let snapshot = Snapshot {
                    last_reading_unix_ns: last_reading,
                    watts: last_watts,
                    lost_events,
                    cost: tariff.as_ref().map(|tariff| tariff.cost(&energy)),
                    energy,
                };
                if let Err(error) = context.publish_status(&snapshot, PUBLISH_EVERY * 3).await {
                    eprintln!("{NAME}: status not published: {error}");
                }
            }
        }
    }
}

/// What the plugin needs from the host, what it publishes, and the console
/// page in `bundle/` that shows it.
fn metadata() -> PluginMetadata {
    PluginMetadata::new(
        NAME,
        env!("CARGO_PKG_VERSION"),
        plugin_server_info(
            NAME,
            env!("CARGO_PKG_VERSION"),
            "Power metrics",
            "Measures the energy this node spends per output token",
            None::<String>,
        ),
    )
    .with_manifest(
        plugin_manifest()
            .item(inference_observation().required(true))
            .item(plugin_status(1).required(true))
            .item(
                web_ui().bundle(web_ui_bundle("energy", "bundle")).page(
                    web_ui_page("energy", "Energy", "energy", "energy.js").bundle_id("energy"),
                ),
            )
            .build(),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Release packaging reads the manifest the installer needs, page included,
    // from the binary itself.
    if std::env::args().nth(1).as_deref() == Some("--print-package-manifest") {
        let manifest = SimplePlugin::new(metadata())
            .manifest()
            .context("plugin manifest")?;
        println!("{}", package_manifest_json(&manifest)?);
        return Ok(());
    }
    let tariff = price::Tariff::from_args(std::env::args().skip(1))?;
    let plugin = SimplePlugin::new(metadata()).on_initialized(move |context| {
        let context = context.owned();
        let tariff = tariff.clone();
        Box::pin(async move {
            tokio::spawn(run(context, tariff));
            Ok(())
        })
    });
    PluginRuntime::run(plugin).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(tariff: Option<price::Tariff>) -> serde_json::Value {
        let energy = meter::EnergyStatusPayload {
            source: "adlx",
            wh_per_1k_output_tokens: Some(0.4),
            wh_per_1k_output_tokens_total: Some(0.6),
            ..meter::EnergyStatusPayload::default()
        };
        serde_json::to_value(Snapshot {
            last_reading_unix_ns: None,
            watts: None,
            lost_events: 0,
            cost: tariff.as_ref().map(|tariff| tariff.cost(&energy)),
            energy,
        })
        .unwrap()
    }

    #[test]
    fn cost_fields_appear_only_with_a_tariff() {
        let plain = snapshot(None);
        assert_eq!(plain["source"], "adlx");
        assert!(plain.get("cost_per_1m_output_tokens").is_none());
        assert!(plain.get("electricity_price_per_kwh").is_none());

        let args = ["--electricity-price-per-kwh", "0.25", "--currency", "EUR"].map(String::from);
        let priced = snapshot(price::Tariff::from_args(args).unwrap());
        assert_eq!(priced["electricity_price_per_kwh"], 0.25);
        assert_eq!(priced["currency"], "EUR");
        assert!((priced["cost_per_1m_output_tokens"].as_f64().unwrap() - 0.1).abs() < 1e-12);
        assert!((priced["cost_per_1m_output_tokens_total"].as_f64().unwrap() - 0.15).abs() < 1e-12);
    }

    /// Release packaging ships `plugin-manifest.json` as checked in: it must be
    /// what the binary declares, and its page script must exist.
    #[test]
    fn the_packaged_manifest_is_the_declared_one() {
        let manifest = SimplePlugin::new(metadata()).manifest().unwrap();
        let declared = package_manifest_json(&manifest).unwrap();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let packaged = std::fs::read_to_string(root.join("plugin-manifest.json")).unwrap();
        assert_eq!(
            packaged.replace("\r\n", "\n").trim_end(),
            declared.trim_end(),
            "regenerate it: cargo run -- --print-package-manifest > plugin-manifest.json"
        );
        assert!(root.join("bundle/energy.js").is_file());
    }
}
