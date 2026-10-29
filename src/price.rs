//! The price floor (mesh-llm #2320, step 2): given what the operator pays for
//! electricity, what a million output tokens cost this node, from the energy
//! the meter measured. Plugins receive their `args`, so the tariff comes from
//! the node config:
//!
//! ```toml
//! [[plugin]]
//! name = "powermetrics-plugin"
//! allow_inference_observation = true
//! args = ["--electricity-price-per-kwh", "0.25", "--currency", "EUR"]
//! ```

use crate::meter::EnergyStatusPayload;
use anyhow::{Context, Result, bail};
use serde::Serialize;

const PRICE: &str = "--electricity-price-per-kwh";
const CURRENCY: &str = "--currency";

/// The operator's electricity price, from the plugin's arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct Tariff {
    price_per_kwh: f64,
    currency: Option<String>,
}

impl Tariff {
    /// `None` when no price is given. Unknown arguments are refused, so a
    /// mistyped flag shows at startup instead of silently pricing nothing.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Option<Self>> {
        let mut price = None;
        let mut currency = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
                None => (arg, None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| args.next())
                    .with_context(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                PRICE => {
                    let text = value()?;
                    let parsed: f64 = text
                        .parse()
                        .with_context(|| format!("{PRICE} is not a number: {text}"))?;
                    if !parsed.is_finite() || parsed <= 0.0 {
                        bail!("{PRICE} must be a positive price, got {text}");
                    }
                    if price.replace(parsed).is_some() {
                        bail!("{PRICE} is given twice");
                    }
                }
                CURRENCY => {
                    let text = value()?;
                    if text.trim().is_empty() {
                        bail!("{CURRENCY} must not be empty");
                    }
                    if currency.replace(text.trim().to_string()).is_some() {
                        bail!("{CURRENCY} is given twice");
                    }
                }
                other => bail!("unknown argument {other}; expected {PRICE} or {CURRENCY}"),
            }
        }
        match (price, currency) {
            (Some(price_per_kwh), currency) => Ok(Some(Self {
                price_per_kwh,
                currency,
            })),
            (None, Some(_)) => bail!("{CURRENCY} needs {PRICE}"),
            (None, None) => Ok(None),
        }
    }

    /// Wh per 1,000 tokens is numerically kWh per million tokens, so each
    /// cost is that rate times the price per kWh.
    pub fn cost(&self, energy: &EnergyStatusPayload) -> CostPayload {
        CostPayload {
            electricity_price_per_kwh: self.price_per_kwh,
            currency: self.currency.clone(),
            cost_per_1m_output_tokens: energy
                .wh_per_1k_output_tokens
                .map(|kwh| kwh * self.price_per_kwh),
            cost_per_1m_output_tokens_total: energy
                .wh_per_1k_output_tokens_total
                .map(|kwh| kwh * self.price_per_kwh),
        }
    }
}

/// What a million output tokens cost at the operator's tariff.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CostPayload {
    pub electricity_price_per_kwh: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// On top of the idle draw: the floor for a machine that runs anyway.
    pub cost_per_1m_output_tokens: Option<f64>,
    /// Idle draw included: the floor for a machine that runs only to serve.
    pub cost_per_1m_output_tokens_total: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_no_tariff() {
        assert_eq!(Tariff::from_args(args(&[])).unwrap(), None);
    }

    #[test]
    fn price_and_currency_parse_as_separate_or_inline_values() {
        let expected = Tariff {
            price_per_kwh: 0.25,
            currency: Some("EUR".into()),
        };
        assert_eq!(
            Tariff::from_args(args(&[PRICE, "0.25", CURRENCY, "EUR"])).unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            Tariff::from_args(args(&[
                "--electricity-price-per-kwh=0.25",
                "--currency=EUR"
            ]))
            .unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn bad_arguments_are_refused_at_startup() {
        for bad in [
            &[PRICE, "0"][..],
            &[PRICE, "-1"],
            &[PRICE, "free"],
            &[PRICE, "NaN"],
            &[PRICE],
            &[CURRENCY, "EUR"],
            &[PRICE, "0.2", CURRENCY, " "],
            &["--electricity-price", "0.2"],
            &[PRICE, "0.2", PRICE, "0.3"],
            &[PRICE, "0.2", CURRENCY, "EUR", CURRENCY, "USD"],
        ] {
            assert!(Tariff::from_args(args(bad)).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn costs_follow_the_measured_rates_and_stay_null_without_them() {
        let tariff = Tariff {
            price_per_kwh: 0.25,
            currency: None,
        };
        let energy = EnergyStatusPayload {
            wh_per_1k_output_tokens: Some(0.4),
            wh_per_1k_output_tokens_total: Some(0.6),
            ..EnergyStatusPayload::default()
        };
        let cost = tariff.cost(&energy);
        assert!((cost.cost_per_1m_output_tokens.unwrap() - 0.1).abs() < 1e-12);
        assert!((cost.cost_per_1m_output_tokens_total.unwrap() - 0.15).abs() < 1e-12);

        let cost = tariff.cost(&EnergyStatusPayload::default());
        assert_eq!(cost.cost_per_1m_output_tokens, None);
        assert_eq!(cost.cost_per_1m_output_tokens_total, None);
    }
}
