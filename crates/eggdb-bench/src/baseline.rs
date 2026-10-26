use std::collections::BTreeMap;
use std::fmt;

pub(crate) const RATIO_TOLERANCE: f64 = 0.30;
const EXACT_EPSILON: f64 = 1e-9;
pub(crate) const ANY_OS: &str = "any";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Exact,
    Ratio,
    Reported,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Metric {
    pub(crate) benchmark: &'static str,
    pub(crate) name: &'static str,
    pub(crate) value: f64,
    pub(crate) gate: Gate,
}

impl Metric {
    pub(crate) fn exact(benchmark: &'static str, name: &'static str, value: f64) -> Self {
        Self {
            benchmark,
            name,
            value,
            gate: Gate::Exact,
        }
    }

    pub(crate) fn ratio(benchmark: &'static str, name: &'static str, value: f64) -> Self {
        Self {
            benchmark,
            name,
            value,
            gate: Gate::Ratio,
        }
    }

    pub(crate) fn reported(benchmark: &'static str, name: &'static str, value: f64) -> Self {
        Self {
            benchmark,
            name,
            value,
            gate: Gate::Reported,
        }
    }

    pub(crate) fn os_key(&self, os: &'static str) -> &'static str {
        match self.gate {
            Gate::Exact => ANY_OS,
            Gate::Ratio | Gate::Reported => os,
        }
    }

    pub(crate) fn formatted_value(&self) -> String {
        format_value(self.gate, self.value)
    }
}

fn format_value(gate: Gate, value: f64) -> String {
    match gate {
        Gate::Exact if value.fract() == 0.0 => format!("{value:.0}"),
        Gate::Exact | Gate::Ratio => format!("{value:.3}"),
        Gate::Reported => format!("{value:.1}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Verdict {
    Within,
    Improved,
    Regressed { limit: f64 },
    Fell { baseline: f64 },
    Missing,
    Reported,
}

impl Verdict {
    pub(crate) fn fails(self, gate: Gate) -> bool {
        match self {
            Self::Regressed { .. } | Self::Fell { .. } => true,
            Self::Missing => gate == Gate::Exact,
            Self::Within | Self::Improved | Self::Reported => false,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Within => f.write_str("ok"),
            Self::Improved => f.write_str("improved: lower the baseline with --update"),
            Self::Regressed { limit } => write!(f, "REGRESSED: above the limit {limit:.3}"),
            Self::Fell { baseline } => write!(
                f,
                "CHANGED: below the baseline {baseline}; counts must match exactly, so if this is intended run --update and say why"
            ),
            Self::Missing => f.write_str("no baseline line"),
            Self::Reported => f.write_str("reported only"),
        }
    }
}

pub(crate) fn judge(metric: &Metric, baseline: Option<f64>) -> Verdict {
    let Some(baseline) = baseline else {
        return if metric.gate == Gate::Reported {
            Verdict::Reported
        } else {
            Verdict::Missing
        };
    };
    match metric.gate {
        Gate::Reported => Verdict::Reported,
        Gate::Exact if metric.value > baseline + EXACT_EPSILON => {
            Verdict::Regressed { limit: baseline }
        }
        Gate::Exact if metric.value < baseline - EXACT_EPSILON => Verdict::Fell { baseline },
        Gate::Exact => Verdict::Within,
        Gate::Ratio => {
            let limit = baseline * (1.0 + RATIO_TOLERANCE);
            if metric.value > limit {
                Verdict::Regressed { limit }
            } else if metric.value < baseline / (1.0 + RATIO_TOLERANCE) {
                Verdict::Improved
            } else {
                Verdict::Within
            }
        }
    }
}

type Key = (String, String, String);

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Baseline {
    values: BTreeMap<Key, f64>,
}

impl Baseline {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.is_empty() {
                continue;
            }
            let [os, benchmark, metric, value] = fields[..] else {
                return Err(format!(
                    "baseline line {}: expected `<os> <benchmark> <metric> <value>`",
                    index + 1
                ));
            };
            let value: f64 = value
                .parse()
                .map_err(|_| format!("baseline line {}: `{value}` is not a number", index + 1))?;
            values.insert((os.into(), benchmark.into(), metric.into()), value);
        }
        Ok(Self { values })
    }

    pub(crate) fn get(&self, os: &str, benchmark: &str, metric: &str) -> Option<f64> {
        self.values
            .get(&(os.into(), benchmark.into(), metric.into()))
            .copied()
    }

    pub(crate) fn replace_os(&mut self, os: &'static str, metrics: &[Metric]) {
        self.values
            .retain(|(line_os, _, _), _| line_os != os && line_os != ANY_OS);
        for metric in metrics
            .iter()
            .filter(|metric| metric.gate != Gate::Reported)
        {
            let key = (
                metric.os_key(os).to_string(),
                metric.benchmark.to_string(),
                metric.name.to_string(),
            );
            self.values.insert(key, metric.value);
        }
    }

    pub(crate) fn render(&self, metrics: &[Metric]) -> String {
        let gate_of = |benchmark: &str, name: &str| {
            metrics
                .iter()
                .find(|metric| metric.benchmark == benchmark && metric.name == name)
                .map_or(Gate::Ratio, |metric| metric.gate)
        };
        self.values
            .iter()
            .map(|((os, benchmark, name), &value)| {
                let value = format_value(gate_of(benchmark, name), value);
                format!("{os} {benchmark} {name} {value}\n")
            })
            .collect()
    }
}

pub(crate) fn line(metric: &Metric, os: &'static str) -> String {
    format!(
        "{} {} {} {}",
        metric.os_key(os),
        metric.benchmark,
        metric.name,
        metric.formatted_value()
    )
}

#[cfg(test)]
mod tests {
    use super::{Baseline, Gate, Metric, Verdict, judge};

    #[test]
    fn exact_counts_fail_on_any_change_and_ratios_only_beyond_the_tolerance() {
        let count = |value| Metric::exact("cache.hit", "allocations_per_op", value);
        assert_eq!(judge(&count(0.0), Some(0.0)), Verdict::Within);
        assert_eq!(
            judge(&count(0.001), Some(0.0)),
            Verdict::Regressed { limit: 0.0 }
        );
        assert_eq!(
            judge(&count(1.0), Some(2.0)),
            Verdict::Fell { baseline: 2.0 }
        );
        assert!(judge(&count(1.0), Some(2.0)).fails(Gate::Exact));
        let ratio = |value| Metric::ratio("cache.hit", "ratio_to_reference", value);
        assert_eq!(judge(&ratio(1.25), Some(1.0)), Verdict::Within);
        assert!(matches!(
            judge(&ratio(1.31), Some(1.0)),
            Verdict::Regressed { .. }
        ));
        assert_eq!(judge(&ratio(0.7), Some(1.0)), Verdict::Improved);
    }

    #[test]
    fn a_missing_count_fails_and_a_missing_ratio_only_warns() {
        let count = Metric::exact("cache.miss", "evictions_per_op", 1.0);
        let ratio = Metric::ratio("cache.hit", "ratio_to_reference", 1.0);
        assert!(judge(&count, None).fails(Gate::Exact));
        assert!(!judge(&ratio, None).fails(Gate::Ratio));
        let reported = Metric::reported("cache.hit", "ns_per_op", 5.0);
        assert_eq!(judge(&reported, None), Verdict::Reported);
    }

    #[test]
    fn an_update_replaces_this_os_and_shared_lines_and_keeps_other_systems() {
        let mut baseline =
            Baseline::parse("linux cache.hit ratio_to_reference 0.5\nmacos cache.hit ratio_to_reference 0.4\nany cache.old allocations_per_op 3\n")
                .unwrap();
        let metrics = [
            Metric::exact("cache.hit", "allocations_per_op", 0.0),
            Metric::ratio("cache.hit", "ratio_to_reference", 0.42),
            Metric::reported("cache.hit", "ns_per_op", 4.0),
        ];
        baseline.replace_os("macos", &metrics);
        assert_eq!(
            baseline.render(&metrics),
            "any cache.hit allocations_per_op 0\nlinux cache.hit ratio_to_reference 0.500\nmacos cache.hit ratio_to_reference 0.420\n"
        );
        assert_eq!(
            Baseline::parse(&baseline.render(&metrics)).unwrap(),
            baseline
        );
    }

    #[test]
    fn malformed_lines_are_refused_with_their_number() {
        assert!(
            Baseline::parse("any a b 1\nany a b\n")
                .unwrap_err()
                .contains("line 2")
        );
        assert!(
            Baseline::parse("any a b one\n")
                .unwrap_err()
                .contains("not a number")
        );
    }
}
