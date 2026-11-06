//! Chart axes: round ticks and short labels for numbers, log scales, dates (UTC), durations and percents.
//!
//! Values on a log axis are kept as their base-10 logarithm, so everything here works in "axis space": ticks are
//! axis-space positions and labels convert back.
use extension_api::{AxisFormat, Scale};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Axis {
    pub scale: Scale,
    pub format: AxisFormat,
}
impl Axis {
    pub(crate) const PLAIN: Self = Self {
        scale: Scale::Linear,
        format: AxisFormat::Number,
    };
    /// A data value in axis space.
    pub(crate) fn to_axis(self, value: f64) -> f64 {
        match self.scale {
            Scale::Linear => value,
            Scale::Log => value.max(f64::MIN_POSITIVE).log10(),
        }
    }
    pub(crate) fn to_value(self, axis: f64) -> f64 {
        match self.scale {
            Scale::Linear => axis,
            Scale::Log => 10f64.powf(axis),
        }
    }
}

/// Ticks for `[min, max]` (axis space) with about `target` of them, and how to write them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Ticks {
    pub values: Vec<f64>,
    step: f64,
    axis: Axis,
}

/// 1, 2 or 5 × 10ⁿ: the round step giving about `target` intervals over `span`.
pub(crate) fn nice_step(span: f64, target: usize) -> f64 {
    let raw = (span / target.max(1) as f64).abs();
    if !raw.is_finite() || raw == 0. {
        return 1.;
    }
    let magnitude = 10f64.powf(raw.log10().floor());
    let fraction = raw / magnitude;
    let nice = if fraction <= 1. {
        1.
    } else if fraction <= 2. {
        2.
    } else if fraction <= 5. {
        5.
    } else {
        10.
    };
    nice * magnitude
}

fn multiples(min: f64, max: f64, step: f64) -> Vec<f64> {
    let first = (min / step - 1e-9).ceil();
    let last = (max / step + 1e-9).floor();
    if !(first.is_finite() && last.is_finite()) || last - first > 200. {
        return vec![min, max];
    }
    (first as i64..=last as i64)
        // -0 written as 0
        .map(|i| i as f64 * step + 0.)
        .collect()
}

const DURATION_STEPS: [f64; 22] = [
    1., 2., 5., 10., 15., 30., 60., 120., 300., 600., 900., 1800., 3600., 7200., 10800., 21600.,
    43200., 86400., 172800., 604800., 1209600., 2592000.,
];
const DAY: f64 = 86400.;

impl Ticks {
    pub(crate) fn new(min: f64, max: f64, axis: Axis, target: usize) -> Self {
        let span = max - min;
        let (values, step) = match (axis.scale, axis.format) {
            (Scale::Log, _) => {
                // Whole decades when the axis spans a few; else round values of the data, written back to logs.
                let decades = span;
                if decades >= 2. {
                    let step = nice_step(decades, target).max(1.).round();
                    (multiples(min, max, step), step)
                } else {
                    // Under two decades, round values of the data; a finer step while that leaves fewer than three.
                    let (low, high) = (axis.to_value(min), axis.to_value(max));
                    let mut chosen = (Vec::new(), 1.);
                    for finer in [target, target * 2, target * 4] {
                        let step = nice_step(high - low, finer);
                        let values: Vec<f64> = multiples(low, high, step)
                            .into_iter()
                            .filter(|v| *v > 0.)
                            .map(|v| v.log10())
                            .collect();
                        let enough = values.len() >= 3;
                        chosen = (values, step);
                        if enough {
                            break;
                        }
                    }
                    chosen
                }
            }
            (_, AxisFormat::Duration) => {
                let raw = span / target.max(1) as f64;
                let step = DURATION_STEPS
                    .iter()
                    .copied()
                    .find(|step| *step >= raw)
                    .unwrap_or_else(|| nice_step(span / DAY, target) * DAY);
                (multiples(min, max, step), step)
            }
            (_, AxisFormat::Date) => date_ticks(min, max, target),
            (_, AxisFormat::Percent) => {
                let step = nice_step(span * 100., target) / 100.;
                (multiples(min, max, step), step)
            }
            (_, AxisFormat::Number) => {
                let step = nice_step(span, target);
                (multiples(min, max, step), step)
            }
        };
        Self { values, step, axis }
    }

    /// Multiples of a given step: the ticks of a domain already rounded out to that step.
    pub(crate) fn with_step(min: f64, max: f64, step: f64, axis: Axis) -> Self {
        Self {
            values: multiples(min, max, step),
            step,
            axis,
        }
    }

    /// The labels, written consistently: one unit suffix and as few decimals as tell them apart.
    pub(crate) fn labels(&self) -> Vec<String> {
        match (self.axis.scale, self.axis.format) {
            (Scale::Log, _) => self
                .values
                .iter()
                .map(|v| exact(self.axis, self.axis.to_value(*v), 3))
                .collect(),
            (_, AxisFormat::Duration) => self.values.iter().map(|v| duration(*v)).collect(),
            (_, AxisFormat::Date) => {
                let style = DateStyle::for_step(self.step);
                self.values.iter().map(|v| date(*v, style)).collect()
            }
            (_, AxisFormat::Percent) => {
                let decimals = decimals(self.step * 100.);
                self.values
                    .iter()
                    .map(|v| format!("{}%", trim(format!("{:.*}", decimals, v * 100.))))
                    .collect()
            }
            (_, AxisFormat::Number) => {
                let largest = self.values.iter().fold(0f64, |m, v| m.max(v.abs()));
                let (divisor, suffix) = suffix(largest);
                if largest != 0. && largest < 1e-3 {
                    return self.values.iter().map(|v| scientific(*v)).collect();
                }
                let decimals = decimals(self.step / divisor);
                self.values
                    .iter()
                    .map(|v| {
                        let text = trim(format!("{:.*}", decimals, v / divisor));
                        if text == "0" {
                            text
                        } else {
                            format!("{text}{suffix}")
                        }
                    })
                    .collect()
            }
        }
    }
}

/// The value of a datum as hover shows it: more precise than a tick, in the axis' own format.
pub(crate) fn exact(axis: Axis, value: f64, significant: usize) -> String {
    match axis.format {
        AxisFormat::Date => date(value, DateStyle::Full),
        AxisFormat::Duration => duration(value),
        AxisFormat::Percent => format!("{}%", significant_digits(value * 100., significant)),
        AxisFormat::Number => {
            let magnitude = value.abs();
            if magnitude >= 1e6 {
                let (divisor, suffix) = suffix(magnitude);
                format!(
                    "{}{suffix}",
                    significant_digits(value / divisor, significant.max(3))
                )
            } else if magnitude != 0. && magnitude < 1e-4 {
                scientific(value)
            } else {
                significant_digits(value, significant)
            }
        }
    }
}

fn significant_digits(value: f64, digits: usize) -> String {
    if value == 0. || !value.is_finite() {
        return "0".into();
    }
    let integer_digits = value.abs().log10().floor() as i32 + 1;
    let decimals = (digits as i32 - integer_digits).clamp(0, 12) as usize;
    trim(format!("{value:.decimals$}"))
}

fn suffix(largest: f64) -> (f64, &'static str) {
    if largest >= 1e12 {
        (1e12, "T")
    } else if largest >= 1e9 {
        (1e9, "B")
    } else if largest >= 1e6 {
        (1e6, "M")
    } else if largest >= 1e4 {
        (1e3, "k")
    } else {
        (1., "")
    }
}

/// Decimals needed to write multiples of `step` exactly.
fn decimals(step: f64) -> usize {
    if step <= 0. || !step.is_finite() {
        return 0;
    }
    let mut decimals = 0;
    let mut scaled = step;
    while decimals < 10 && (scaled - scaled.round()).abs() > 1e-6 * scaled.abs().max(1.) {
        scaled *= 10.;
        decimals += 1;
    }
    decimals
}

fn trim(text: String) -> String {
    if text.contains('.') {
        let text = text.trim_end_matches('0').trim_end_matches('.');
        if text == "-0" {
            "0".into()
        } else {
            text.into()
        }
    } else {
        text
    }
}

fn scientific(value: f64) -> String {
    if value == 0. {
        return "0".into();
    }
    let text = format!("{value:.1e}");
    // 1.0e-6 → 1e-6
    text.replace(".0e", "e")
}

pub(crate) fn duration(seconds: f64) -> String {
    let negative = seconds < 0.;
    let total = seconds.abs();
    let sign = if negative { "-" } else { "" };
    let text = if total < 60. {
        format!("{}s", trim(format!("{total:.1}")))
    } else if total < 3600. {
        let minutes = (total / 60.).floor();
        let rest = (total - minutes * 60.).round();
        if rest == 0. {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{rest:02}")
        }
    } else if total < 2. * DAY {
        let hours = (total / 3600.).floor();
        let minutes = ((total - hours * 3600.) / 60.).round();
        if minutes == 0. {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes:02}")
        }
    } else {
        let days = (total / DAY).floor();
        let hours = ((total - days * DAY) / 3600.).round();
        if hours == 0. {
            format!("{days}d")
        } else {
            format!("{days}d{hours}h")
        }
    };
    format!("{sign}{text}")
}

// --- dates (UTC: the protocol's x is Unix seconds, with no zone)

#[derive(Clone, Copy, Debug, PartialEq)]
enum DateStyle {
    Seconds,
    Minutes,
    Days,
    Months,
    Years,
    Full,
}
impl DateStyle {
    fn for_step(step: f64) -> Self {
        if step < 60. {
            Self::Seconds
        } else if step < DAY {
            Self::Minutes
        } else if step < 28. * DAY {
            Self::Days
        } else if step < 365. * DAY {
            Self::Months
        } else {
            Self::Years
        }
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// (year, month 1..=12, day 1..=31) of a day count since 1970-01-01 (Howard Hinnant's civil_from_days).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}
/// Days since 1970-01-01 of a civil date (days_from_civil).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn date(seconds: f64, style: DateStyle) -> String {
    let whole = seconds.floor() as i64;
    let days = whole.div_euclid(86_400);
    let time = whole.rem_euclid(86_400);
    let (year, month, day) = civil(days);
    let (hour, minute, second) = (time / 3600, time % 3600 / 60, time % 60);
    let name = MONTHS[month as usize - 1];
    match style {
        DateStyle::Seconds => format!("{hour:02}:{minute:02}:{second:02}"),
        DateStyle::Minutes if time == 0 => format!("{name} {day}"),
        DateStyle::Minutes => format!("{hour:02}:{minute:02}"),
        DateStyle::Days => format!("{name} {day}"),
        DateStyle::Months if month == 1 => format!("{name} {year}"),
        DateStyle::Months => name.to_owned(),
        DateStyle::Years => year.to_string(),
        DateStyle::Full if time == 0 => format!("{year}-{month:02}-{day:02}"),
        DateStyle::Full => format!("{year}-{month:02}-{day:02} {hour:02}:{minute:02}"),
    }
}

fn date_ticks(min: f64, max: f64, target: usize) -> (Vec<f64>, f64) {
    let raw = (max - min) / target.max(1) as f64;
    const SUB_DAY: [f64; 15] = [
        1., 5., 15., 30., 60., 300., 900., 1800., 3600., 10800., 21600., 43200., 86400., 172800.,
        604800.,
    ];
    if let Some(step) = SUB_DAY.iter().copied().find(|step| *step >= raw)
        && step <= 7. * DAY
    {
        if step == 7. * DAY {
            // Mondays: 1970-01-05 was one.
            let offset = 4. * DAY;
            let values = multiples(min - offset, max - offset, step)
                .into_iter()
                .map(|v| v + offset)
                .collect();
            return (values, step);
        }
        return (multiples(min, max, step), step);
    }
    // Calendar months: 1, 3 or 6 of them, then whole years.
    let months = [1, 3, 6, 12, 24, 60, 120]
        .into_iter()
        .find(|months| *months as f64 * 30.44 * DAY >= raw)
        .unwrap_or(120);
    let (start_year, start_month, _) = civil((min / DAY).floor() as i64);
    let mut index = start_year * 12 + i64::from(start_month) - 1;
    index -= index.rem_euclid(months);
    let mut values = Vec::new();
    loop {
        let (year, month) = (index.div_euclid(12), index.rem_euclid(12) as u32 + 1);
        let at = days_from_civil(year, month, 1) as f64 * DAY;
        if at > max || values.len() > 200 {
            break;
        }
        if at >= min {
            values.push(at);
        }
        index += months;
    }
    (values, months as f64 * 30.44 * DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(min: f64, max: f64, axis: Axis) -> Vec<String> {
        Ticks::new(axis.to_axis(min), axis.to_axis(max), axis, 5).labels()
    }

    #[test]
    fn number_ticks_are_round_and_short() {
        assert_eq!(
            labels(1., 500., Axis::PLAIN),
            ["100", "200", "300", "400", "500"]
        );
        assert_eq!(
            labels(0., 885_000., Axis::PLAIN),
            ["0", "200k", "400k", "600k", "800k"]
        );
        assert_eq!(
            labels(11264., 94129.5, Axis::PLAIN),
            ["20k", "40k", "60k", "80k"]
        );
        assert_eq!(labels(0.21, 0.95, Axis::PLAIN), ["0.4", "0.6", "0.8"]);
        assert_eq!(labels(0., 1.2e6, Axis::PLAIN), ["0", "0.5M", "1M"]);
        assert_eq!(labels(0., 4e-6, Axis::PLAIN)[1], "1e-6");
        assert_eq!(
            labels(-1., 1., Axis::PLAIN),
            ["-1", "-0.5", "0", "0.5", "1"]
        );
    }

    #[test]
    fn log_ticks_are_decades_or_round_values() {
        let log = Axis {
            scale: Scale::Log,
            format: AxisFormat::Number,
        };
        assert_eq!(labels(0.001, 10., log), ["0.001", "0.01", "0.1", "1", "10"]);
        assert_eq!(labels(2., 9., log), ["2", "4", "6", "8"]);
        // A short log span still gets three ticks or more.
        let ticks = Ticks::new(0.3f64.log10(), 2f64.log10(), log, 3);
        assert!(ticks.labels().len() >= 3, "{:?}", ticks.labels());
    }

    #[test]
    fn durations_dates_and_percents() {
        let format = |format| Axis {
            scale: Scale::Linear,
            format,
        };
        assert_eq!(
            labels(0., 7200., format(AxisFormat::Duration)),
            ["0s", "30m", "1h", "1h30", "2h"]
        );
        assert_eq!(duration(45.), "45s");
        assert_eq!(duration(12. * 60.), "12m");
        assert_eq!(duration(3. * 3600. + 20. * 60.), "3h20");
        assert_eq!(
            labels(0., 1., format(AxisFormat::Percent)),
            ["0%", "20%", "40%", "60%", "80%", "100%"]
        );
        // 2026-01-01 .. 2026-06-01: months, the year with January.
        let start = days_from_civil(2026, 1, 1) as f64 * DAY;
        let end = days_from_civil(2026, 6, 1) as f64 * DAY;
        assert_eq!(
            labels(start, end, format(AxisFormat::Date)),
            ["Jan 2026", "Feb", "Mar", "Apr", "May", "Jun"]
        );
        assert_eq!(civil(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(
            exact(format(AxisFormat::Date), start + 3600. * 13.5, 4),
            "2026-01-01 13:30"
        );
        let days = labels(start, start + 5. * DAY, format(AxisFormat::Date));
        assert_eq!(days.first().map(String::as_str), Some("Jan 1"));
    }

    #[test]
    fn exact_values_keep_precision_without_noise() {
        assert_eq!(exact(Axis::PLAIN, 0.812345678, 4), "0.8123");
        assert_eq!(exact(Axis::PLAIN, 885000., 4), "885000");
        assert_eq!(exact(Axis::PLAIN, 1234567., 4), "1.235M");
        assert_eq!(exact(Axis::PLAIN, 3., 4), "3");
    }
}
