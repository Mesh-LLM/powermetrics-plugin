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

mod meter;
mod sampler;
mod settle;

use mesh_llm_plugin::inference_observation::{InferenceObservationItem, inference_observation};
use mesh_llm_plugin::plugin_status::plugin_status;
use mesh_llm_plugin::{
    PluginContext, PluginMetadata, PluginRuntime, SimplePlugin, plugin_manifest, plugin_server_info,
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
    /// Events the host dropped while this plugin read too slowly; the work
    /// running through a loss is left out of the rates.
    lost_events: u64,
    #[serde(flatten)]
    energy: meter::EnergyStatusPayload,
}

async fn run(mut context: PluginContext<'static>) {
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
                settler.reading(at, watts);
            }
            _ = publish.tick() => {
                let Some(source) = source else { continue };
                let snapshot = Snapshot {
                    last_reading_unix_ns: last_reading,
                    lost_events,
                    energy: settler.meter.status(source),
                };
                if let Err(error) = context.publish_status(&snapshot, PUBLISH_EVERY * 3).await {
                    eprintln!("{NAME}: status not published: {error}");
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = PluginMetadata::new(
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
            .build(),
    );
    let plugin = SimplePlugin::new(metadata).on_initialized(|context| {
        let context = context.owned();
        Box::pin(async move {
            tokio::spawn(run(context));
            Ok(())
        })
    });
    PluginRuntime::run(plugin).await
}
