use std::collections::BTreeMap;
use std::fmt;

const RATIO_TOLERANCE_PERCENT: u32 = 30;
const EXACT_EPSILON: f64 = 1e-9;
pub(crate) const BLOCKING_FACTOR: f64 = 2.0;
pub(crate) const ANY_OS: &str = "any";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Exact,
    Ceiling { tolerance_percent: u32 },
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
        Self::ceiling(benchmark, name, value, RATIO_TOLERANCE_PERCENT)
    }

    pub(crate) fn ceiling(
        benchmark: &'static str,
        name: &'static str,
        value: f64,
        tolerance_percent: u32,
    ) -> Self {
        Self {
            benchmark,
            name,
            value,
            gate: Gate::Ceiling { tolerance_percent },
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
            Gate::Ceiling { .. } | Gate::Reported => os,
        }
    }

    pub(crate) fn formatted_value(&self) -> String {
        format_value(self.gate, self.value)
    }
}

fn format_value(gate: Gate, value: f64) -> String {
    match gate {
        Gate::Exact | Gate::Ceiling { .. } if value.fract() == 0.0 => format!("{value:.0}"),
        Gate::Exact | Gate::Ceiling { .. } => format!("{value:.3}"),
        Gate::Reported if value.abs() < 10.0 => format!("{value:.3}"),
        Gate::Reported => format!("{value:.1}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Verdict {
    Within,
    Improved,
    Slower { limit: f64 },
    Blocked { limit: f64 },
    Missing,
    Reported,
}

impl Verdict {
    pub(crate) fn fails(self, gate: Gate) -> bool {
        match self {
            Self::Blocked { .. } => true,
            Self::Missing => gate == Gate::Exact,
            Self::Within | Self::Improved | Self::Slower { .. } | Self::Reported => false,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Within => "ok",
            Self::Improved => "improved",
            Self::Slower { .. } => "slower",
            Self::Blocked { .. } => "blocked",
            Self::Missing => "missing",
            Self::Reported => "reported",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Within => f.write_str("ok"),
            Self::Improved => f.write_str("improved: lower the baseline with --update"),
            Self::Slower { limit } => write!(
                f,
                "SLOWER: above the limit {limit:.3}; reported, the weekly performance pass fixes it"
            ),
            Self::Blocked { limit } => write!(
                f,
                "BLOCKED: more than {BLOCKING_FACTOR}x the baseline (limit {limit:.3}); fix it in this change"
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
    let blocking_limit = baseline * BLOCKING_FACTOR;
    match metric.gate {
        Gate::Reported => Verdict::Reported,
        Gate::Exact if metric.value > blocking_limit + EXACT_EPSILON => Verdict::Blocked {
            limit: blocking_limit,
        },
        Gate::Exact if metric.value > baseline + EXACT_EPSILON => {
            Verdict::Slower { limit: baseline }
        }
        Gate::Exact if metric.value < baseline - EXACT_EPSILON => Verdict::Improved,
        Gate::Exact => Verdict::Within,
        Gate::Ceiling { tolerance_percent } => {
            let factor = 1.0 + f64::from(tolerance_percent) / 100.0;
            let limit = baseline * factor;
            if metric.value > blocking_limit {
                Verdict::Blocked {
                    limit: blocking_limit,
                }
            } else if metric.value > limit {
                Verdict::Slower { limit }
            } else if metric.value < baseline / factor {
                Verdict::Improved
            } else {
                Verdict::Within
            }
        }
    }
}

pub(crate) fn delta_text(value: f64, baseline: Option<f64>) -> String {
    match baseline {
        None => "-".to_string(),
        Some(baseline) if baseline.abs() < EXACT_EPSILON && value.abs() < EXACT_EPSILON => {
            "0%".to_string()
        }
        Some(baseline) if baseline.abs() < EXACT_EPSILON => "new".to_string(),
        Some(baseline) => format!("{:+.1}%", (value / baseline - 1.0) * 100.0),
    }
}

type Key = (String, String, String);

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    value: f64,
    text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Baseline {
    entries: BTreeMap<Key, Entry>,
}

impl Baseline {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let mut entries = BTreeMap::new();
        for (index, line) in text.lines().enumerate() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.is_empty() {
                continue;
            }
            let [os, benchmark, metric, text] = fields[..] else {
                return Err(format!(
                    "baseline line {}: expected `<os> <benchmark> <metric> <value>`",
                    index + 1
                ));
            };
            let value: f64 = text
                .parse()
                .map_err(|_| format!("baseline line {}: `{text}` is not a number", index + 1))?;
            let entry = Entry {
                value,
                text: text.into(),
            };
            entries.insert((os.into(), benchmark.into(), metric.into()), entry);
        }
        Ok(Self { entries })
    }

    pub(crate) fn get(&self, os: &str, benchmark: &str, metric: &str) -> Option<f64> {
        self.entries
            .get(&(os.into(), benchmark.into(), metric.into()))
            .map(|entry| entry.value)
    }

    pub(crate) fn replace_os(&mut self, os: &'static str, metrics: &[Metric]) {
        let measured_family = |benchmark: &str| {
            metrics
                .iter()
                .any(|metric| family(metric.benchmark) == family(benchmark))
        };
        self.entries.retain(|(line_os, benchmark, _), _| {
            (line_os != os && line_os != ANY_OS) || !measured_family(benchmark)
        });
        for metric in metrics
            .iter()
            .filter(|metric| metric.gate != Gate::Reported)
        {
            let key = (
                metric.os_key(os).to_string(),
                metric.benchmark.to_string(),
                metric.name.to_string(),
            );
            let entry = Entry {
                value: metric.value,
                text: metric.formatted_value(),
            };
            self.entries.insert(key, entry);
        }
    }

    pub(crate) fn render(&self) -> String {
        self.entries
            .iter()
            .map(|((os, benchmark, name), entry)| {
                format!("{os} {benchmark} {name} {}\n", entry.text)
            })
            .collect()
    }
}

fn family(benchmark: &str) -> &str {
    benchmark
        .split_once('.')
        .map_or(benchmark, |(family, _)| family)
}

pub(crate) fn result_line(
    metric: &Metric,
    os: &'static str,
    baseline: Option<f64>,
    verdict: Verdict,
) -> String {
    let expected = baseline.map_or_else(|| "-".to_string(), |value| format!("{value}"));
    let delta = delta_text(metric.value, baseline);
    format!(
        "{} {} {} {} {expected} {delta} {}",
        os,
        metric.benchmark,
        metric.name,
        metric.formatted_value(),
        verdict.label()
    )
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
    use super::{Baseline, Gate, Metric, Verdict, delta_text, judge, result_line};

    #[test]
    fn a_count_above_its_line_is_reported_up_to_twice_the_line_and_blocks_beyond() {
        let count = |value| Metric::exact("cache.hit", "file_operations_per_op", value);
        assert_eq!(judge(&count(4.0), Some(4.0)), Verdict::Within);
        assert_eq!(
            judge(&count(5.0), Some(4.0)),
            Verdict::Slower { limit: 4.0 }
        );
        assert_eq!(
            judge(&count(8.0), Some(4.0)),
            Verdict::Slower { limit: 4.0 }
        );
        assert_eq!(
            judge(&count(8.5), Some(4.0)),
            Verdict::Blocked { limit: 8.0 }
        );
        assert!(!judge(&count(8.0), Some(4.0)).fails(Gate::Exact));
        assert!(judge(&count(8.5), Some(4.0)).fails(Gate::Exact));
        assert_eq!(judge(&count(3.0), Some(4.0)), Verdict::Improved);
        assert!(!judge(&count(3.0), Some(4.0)).fails(Gate::Exact));
    }

    #[test]
    fn any_count_above_a_line_of_zero_blocks() {
        let count = |value| Metric::exact("cache.hit", "allocations_per_op", value);
        assert_eq!(judge(&count(0.0), Some(0.0)), Verdict::Within);
        assert_eq!(
            judge(&count(1.0), Some(0.0)),
            Verdict::Blocked { limit: 0.0 }
        );
    }

    #[test]
    fn a_ratio_slower_than_its_tolerance_is_reported_and_more_than_twice_the_line_blocks() {
        let ratio = |value| Metric::ratio("cache.hit", "ratio_to_reference", value);
        assert_eq!(judge(&ratio(1.25), Some(1.0)), Verdict::Within);
        assert_eq!(
            judge(&ratio(1.31), Some(1.0)),
            Verdict::Slower { limit: 1.3 }
        );
        assert_eq!(
            judge(&ratio(2.0), Some(1.0)),
            Verdict::Slower { limit: 1.3 }
        );
        assert_eq!(
            judge(&ratio(2.01), Some(1.0)),
            Verdict::Blocked { limit: 2.0 }
        );
        assert!(judge(&ratio(2.01), Some(1.0)).fails(Gate::Ceiling {
            tolerance_percent: 30
        }));
        assert!(!judge(&ratio(2.0), Some(1.0)).fails(Gate::Ceiling {
            tolerance_percent: 30
        }));
        assert_eq!(judge(&ratio(0.7), Some(1.0)), Verdict::Improved);
    }

    #[test]
    fn a_ceiling_has_its_own_tolerance_and_the_same_blocking_limit() {
        let size = |value| Metric::ceiling("footprint.binary", "bytes", value, 1);
        assert_eq!(
            judge(&size(1_009_000.0), Some(1_000_000.0)),
            Verdict::Within
        );
        assert_eq!(
            judge(&size(1_011_000.0), Some(1_000_000.0)),
            Verdict::Slower { limit: 1_010_000.0 }
        );
        assert_eq!(
            judge(&size(2_000_001.0), Some(1_000_000.0)),
            Verdict::Blocked { limit: 2_000_000.0 }
        );
        assert_eq!(
            judge(&size(980_000.0), Some(1_000_000.0)),
            Verdict::Improved
        );
        assert_eq!(size(933_312.0).formatted_value(), "933312");
    }

    #[test]
    fn deltas_are_signed_percentages_against_the_line() {
        assert_eq!(delta_text(1.25, Some(1.0)), "+25.0%");
        assert_eq!(delta_text(0.5, Some(1.0)), "-50.0%");
        assert_eq!(delta_text(3.0, None), "-");
        assert_eq!(delta_text(0.0, Some(0.0)), "0%");
        assert_eq!(delta_text(2.0, Some(0.0)), "new");
    }

    #[test]
    fn a_result_line_carries_the_line_the_change_and_a_one_word_verdict() {
        let ratio = Metric::ratio("cache.hit", "ratio_to_reference", 1.5);
        let verdict = judge(&ratio, Some(1.0));
        assert_eq!(
            result_line(&ratio, "linux", Some(1.0), verdict),
            "linux cache.hit ratio_to_reference 1.500 1 +50.0% slower"
        );
        let reported = Metric::reported("cache.hit", "ns_per_op", 5.0);
        assert_eq!(
            result_line(&reported, "linux", None, judge(&reported, None)),
            "linux cache.hit ns_per_op 5.000 - - reported"
        );
    }

    #[test]
    fn a_missing_count_fails_and_a_missing_ratio_only_warns() {
        let count = Metric::exact("cache.miss", "evictions_per_op", 1.0);
        let ratio = Metric::ratio("cache.hit", "ratio_to_reference", 1.0);
        assert!(judge(&count, None).fails(Gate::Exact));
        assert!(!judge(&ratio, None).fails(ratio.gate));
        let reported = Metric::reported("cache.hit", "ns_per_op", 5.0);
        assert_eq!(judge(&reported, None), Verdict::Reported);
    }

    #[test]
    fn an_update_replaces_this_os_and_shared_lines_of_the_measured_families_only() {
        let mut baseline = Baseline::parse(
            "linux cache.hit ratio_to_reference 0.5\nmacos cache.hit ratio_to_reference 0.4\nany cache.old allocations_per_op 3\nmacos other.get ratio_to_x 1.0\nany other.get allocations_per_op 205941\n",
        )
        .unwrap();
        let metrics = [
            Metric::exact("cache.hit", "allocations_per_op", 0.0),
            Metric::ratio("cache.hit", "ratio_to_reference", 0.42),
            Metric::reported("cache.hit", "ns_per_op", 4.0),
        ];
        baseline.replace_os("macos", &metrics);
        assert_eq!(
            baseline.render(),
            "any cache.hit allocations_per_op 0\nany other.get allocations_per_op 205941\nlinux cache.hit ratio_to_reference 0.5\nmacos cache.hit ratio_to_reference 0.420\nmacos other.get ratio_to_x 1.0\n"
        );
        assert_eq!(Baseline::parse(&baseline.render()).unwrap(), baseline);
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
