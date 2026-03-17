//! CC Economics: compute dollar savings from token reduction.

use serde::{Deserialize, Serialize};

/// API pricing rates (per million tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub model: String,
    /// USD per million input tokens.
    pub input_rate: f64,
    /// USD per million output tokens.
    pub output_rate: f64,
    /// USD per million cache-read tokens.
    pub cache_read_rate: f64,
    /// USD per million cache-write tokens.
    pub cache_write_rate: f64,
}

/// Well-known pricing tiers (as of early 2026).
pub fn known_pricing(model: &str) -> ModelPricing {
    match model {
        "opus" | "claude-opus-4-6" => ModelPricing {
            model: model.to_string(),
            input_rate: 15.0,
            output_rate: 75.0,
            cache_read_rate: 1.5,
            cache_write_rate: 18.75,
        },
        "sonnet" | "claude-sonnet-4-6" => ModelPricing {
            model: model.to_string(),
            input_rate: 3.0,
            output_rate: 15.0,
            cache_read_rate: 0.3,
            cache_write_rate: 3.75,
        },
        "haiku" | "claude-haiku-4-5" => ModelPricing {
            model: model.to_string(),
            input_rate: 0.80,
            output_rate: 4.0,
            cache_read_rate: 0.08,
            cache_write_rate: 1.0,
        },
        _ => ModelPricing {
            model: model.to_string(),
            input_rate: 3.0,
            output_rate: 15.0,
            cache_read_rate: 0.3,
            cache_write_rate: 3.75,
        },
    }
}

/// Compute the dollar savings from reducing tokens.
pub fn compute_savings_value(saved_tokens: u64, model: &str) -> f64 {
    let pricing = known_pricing(model);
    // Saved tokens are input tokens that don't need to be sent
    (saved_tokens as f64 / 1_000_000.0) * pricing.input_rate
}

/// Format a dollar amount.
pub fn format_usd(value: f64) -> String {
    if value < 0.01 {
        format!("${:.4}", value)
    } else if value < 1.0 {
        format!("${:.3}", value)
    } else {
        format!("${:.2}", value)
    }
}

/// Format cost per thousand tokens.
pub fn format_cpt(tokens: u64, cost: f64) -> String {
    if tokens == 0 {
        return "$0.000/1k".to_string();
    }
    let cpt = (cost / tokens as f64) * 1000.0;
    format!("${:.4}/1k", cpt)
}

/// Format token count with K/M suffixes.
pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        format!("{tokens}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_savings_opus() {
        // 1M tokens saved at $15/1M = $15
        let savings = compute_savings_value(1_000_000, "opus");
        assert!((savings - 15.0).abs() < 0.01);
    }

    #[test]
    fn compute_savings_sonnet() {
        let savings = compute_savings_value(1_000_000, "sonnet");
        assert!((savings - 3.0).abs() < 0.01);
    }

    #[test]
    fn format_usd_various() {
        assert_eq!(format_usd(0.001), "$0.0010");
        assert_eq!(format_usd(0.15), "$0.150");
        assert_eq!(format_usd(12.50), "$12.50");
    }

    #[test]
    fn format_tokens_various() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }
}
