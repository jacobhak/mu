//! The `chart` tool: the model hands over data and the UI draws it for the user. Drawing needs the
//! terminal's width, so it happens where the result is shown, from the same arguments (`parse`); the
//! model gets back only a summary of what was shown.

use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_SERIES: usize = 12;
const MAX_POINTS: usize = 5000;
const MAX_BARS: usize = 60;
const MAX_VALUES: usize = 400;
const MIN_HEIGHT: usize = 4;
const MAX_HEIGHT: usize = 40;
const DEFAULT_HEIGHT: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Line,
    Scatter,
    Time,
    Bar,
    Spark,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Line => "line",
            Kind::Scatter => "scatter",
            Kind::Time => "time",
            Kind::Bar => "bar",
            Kind::Spark => "spark",
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum X {
    Num(f64),
    Text(String),
}

#[derive(Deserialize)]
struct SeriesArg {
    name: Option<String>,
    points: Vec<(X, f64)>,
}

#[derive(Deserialize)]
struct BarArg {
    label: String,
    value: f64,
}

#[derive(Deserialize)]
pub struct Args {
    kind: Kind,
    title: Option<String>,
    #[serde(default)]
    series: Vec<SeriesArg>,
    #[serde(default)]
    bars: Vec<BarArg>,
    #[serde(default)]
    values: Vec<f64>,
    height: Option<usize>,
}

/// One series, with x resolved to a number (unix seconds for a time chart).
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    pub name: String,
    pub points: Vec<(f64, f64)>,
}

/// A validated chart.
#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    pub kind: Kind,
    pub title: Option<String>,
    pub series: Vec<Series>,
    pub bars: Vec<(String, f64)>,
    pub values: Vec<f64>,
    pub height: usize,
}

pub fn description() -> &'static str {
    include_str!("../prompts/chart-tool.md")
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "kind":{"type":"string","enum":["line","scatter","time","bar","spark"],
            "description":"line/scatter/time use `series`, bar uses `bars`, spark uses `values`"},
        "title":{"type":"string"},
        "series":{"type":"array","maxItems":MAX_SERIES,"items":{"type":"object","properties":{
            "name":{"type":"string"},
            "points":{"type":"array","items":{"type":"array","minItems":2,"maxItems":2,
                "description":"[x, y]; for kind time x is unix seconds or an ISO 8601 date or timestamp string"}}},
            "required":["points"]}},
        "bars":{"type":"array","maxItems":MAX_BARS,"items":{"type":"object","properties":{
            "label":{"type":"string"},"value":{"type":"number","minimum":0}},"required":["label","value"]}},
        "values":{"type":"array","maxItems":MAX_VALUES,"items":{"type":"number"}},
        "height":{"type":"integer","minimum":MIN_HEIGHT,"maximum":MAX_HEIGHT,"description":"rows; default 12"}},
        "required":["kind"]})
}

/// Unix seconds of an ISO 8601 timestamp, date-time or date (a bare one is read in the local zone).
fn time_of(s: &str) -> Option<f64> {
    let s = s.trim();
    let zone = jiff::tz::TimeZone::system();
    let ts = s
        .parse::<jiff::Timestamp>()
        .ok()
        .or_else(|| s.parse::<jiff::civil::DateTime>().ok()?.to_zoned(zone.clone()).ok().map(|z| z.timestamp()))
        .or_else(|| s.parse::<jiff::civil::Date>().ok()?.to_zoned(zone).ok().map(|z| z.timestamp()))?;
    Some(ts.as_second() as f64)
}

fn x_of(x: X, kind: Kind, series: &str) -> Result<f64, String> {
    let v = match x {
        X::Num(v) if kind == Kind::Time && v.abs() >= 1e11 => v / 1000.0,
        X::Num(v) => v,
        X::Text(s) if kind == Kind::Time => {
            time_of(&s).ok_or_else(|| format!("series {series}: `{s}` is not an ISO 8601 date or timestamp"))?
        }
        X::Text(s) => return Err(format!("series {series}: x `{s}` is text; only kind time takes dates")),
    };
    if v.is_finite() { Ok(v) } else { Err(format!("series {series}: x is not a finite number")) }
}

/// Checks the arguments and resolves them into a chart; an error names what to fix.
pub fn resolve(a: Args) -> Result<Chart, String> {
    let height = a.height.unwrap_or(DEFAULT_HEIGHT).clamp(MIN_HEIGHT, MAX_HEIGHT);
    let mut c = Chart {
        kind: a.kind,
        title: a.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
        series: Vec::new(),
        bars: Vec::new(),
        values: Vec::new(),
        height,
    };
    match a.kind {
        Kind::Line | Kind::Scatter | Kind::Time => {
            if a.series.is_empty() {
                return Err(format!("kind {} needs `series`", a.kind.name()));
            }
            if a.series.len() > MAX_SERIES {
                return Err(format!("at most {MAX_SERIES} series"));
            }
            if a.series.iter().map(|s| s.points.len()).sum::<usize>() > MAX_POINTS {
                return Err(format!("at most {MAX_POINTS} points in all: downsample or aggregate first"));
            }
            for (i, s) in a.series.into_iter().enumerate() {
                let name = s.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("series {}", i + 1));
                if s.points.is_empty() {
                    return Err(format!("series {name} has no points"));
                }
                let mut points = Vec::with_capacity(s.points.len());
                for (x, y) in s.points {
                    if !y.is_finite() {
                        return Err(format!("series {name}: y is not a finite number"));
                    }
                    points.push((x_of(x, a.kind, &name)?, y));
                }
                if a.kind == Kind::Time {
                    points.sort_by(|p, q| p.0.total_cmp(&q.0));
                }
                c.series.push(Series { name, points });
            }
        }
        Kind::Bar => {
            if a.bars.is_empty() {
                return Err("kind bar needs `bars`".into());
            }
            if a.bars.len() > MAX_BARS {
                return Err(format!("at most {MAX_BARS} bars"));
            }
            for b in a.bars {
                if !b.value.is_finite() || b.value < 0.0 {
                    return Err(format!("bar {}: value must be a number of at least 0", b.label));
                }
                c.bars.push((b.label, b.value));
            }
        }
        Kind::Spark => {
            if a.values.is_empty() {
                return Err("kind spark needs `values`".into());
            }
            if a.values.len() > MAX_VALUES {
                return Err(format!("at most {MAX_VALUES} values"));
            }
            if a.values.iter().any(|v| !v.is_finite()) {
                return Err("values must be finite numbers".into());
            }
            c.values = a.values;
        }
    }
    Ok(c)
}

/// The chart a call's arguments describe.
pub fn parse(args: &Value) -> Result<Chart, String> {
    let a: Args = serde_json::from_value(args.clone()).map_err(|e| format!("Invalid arguments for chart: {e}"))?;
    resolve(a)
}

fn num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}").trim_end_matches('0').to_string()
    }
}

fn range(it: impl Iterator<Item = f64>) -> String {
    let (lo, hi) = it.fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| (l.min(v), h.max(v)));
    format!("{}..{}", num(lo), num(hi))
}

impl Chart {
    /// What the model is told: that the user saw the chart, and its extent.
    pub fn summary(&self) -> String {
        let title = self.title.as_deref().map(|t| format!(" \"{t}\"")).unwrap_or_default();
        let what = match self.kind {
            Kind::Bar => {
                format!("{} bars, values {}", self.bars.len(), range(self.bars.iter().map(|b| b.1)))
            }
            Kind::Spark => format!("{} values, {}", self.values.len(), range(self.values.iter().copied())),
            _ => {
                let pts = || self.series.iter().flat_map(|s| s.points.iter());
                let x = if self.kind == Kind::Time {
                    "time".to_string()
                } else {
                    format!("x {}", range(pts().map(|p| p.0)))
                };
                format!("{} series, {} points; {x}, y {}", self.series.len(), pts().count(), range(pts().map(|p| p.1)))
            }
        };
        format!(
            "Chart{title} shown to the user ({}: {what}). They see it as drawn; do not repeat the data.",
            self.kind.name()
        )
    }
}

pub fn run(a: Args) -> ToolOutput {
    match resolve(a) {
        Ok(c) => ToolOutput::ok(c.summary()),
        Err(e) => ToolOutput::err(format!("Invalid arguments for chart: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(v: Value) -> Result<Chart, String> {
        parse(&v)
    }

    #[test]
    fn a_line_chart_resolves_and_names_its_series() {
        let c =
            chart(json!({"kind":"line","series":[{"points":[[0,1],[1,3]]},{"name":"b","points":[[0,2]]}]})).unwrap();
        assert_eq!(c.series[0].name, "series 1");
        assert_eq!(c.series[1].name, "b");
        assert_eq!(c.height, 12);
        let s = c.summary();
        assert!(s.contains("2 series, 3 points; x 0..1, y 1..3"), "{s}");
    }

    #[test]
    fn time_accepts_seconds_millis_and_iso_and_sorts() {
        let c = chart(json!({"kind":"time","series":[{"points":[
            ["2025-01-02T00:00:00Z", 2], [1735689600, 1], [1735776000000i64, 3]]}]}))
        .unwrap();
        let xs: Vec<f64> = c.series[0].points.iter().map(|p| p.0).collect();
        assert_eq!(xs, [1735689600.0, 1735776000.0, 1735776000.0]);
        assert!(chart(json!({"kind":"time","series":[{"points":[["2025-01-31", 1]]}]})).is_ok());
        let e = chart(json!({"kind":"time","series":[{"points":[["soon", 1]]}]})).unwrap_err();
        assert!(e.contains("not an ISO 8601"), "{e}");
    }

    #[test]
    fn bad_input_names_the_fix() {
        let err = |v| chart(v).unwrap_err();
        assert!(err(json!({"kind":"line"})).contains("needs `series`"));
        assert!(err(json!({"kind":"line","series":[{"points":[["a",1]]}]})).contains("only kind time"));
        assert!(err(json!({"kind":"bar","bars":[{"label":"a","value":-1}]})).contains("at least 0"));
        assert!(err(json!({"kind":"spark"})).contains("needs `values`"));
        assert!(err(json!({"kind":"pie"})).contains("Invalid arguments"));
        let many: Vec<[i32; 2]> = (0..5001).map(|i| [i, i]).collect();
        assert!(err(json!({"kind":"line","series":[{"points":many}]})).contains("at most 5000"));
    }

    #[test]
    fn height_is_clamped_and_the_summary_is_what_the_model_sees() {
        let c = chart(json!({"kind":"bar","title":"Weekly","height":900,"bars":[{"label":"a","value":2},{"label":"b","value":5.5}]}))
            .unwrap();
        assert_eq!(c.height, MAX_HEIGHT);
        let s = c.summary();
        assert!(s.starts_with("Chart \"Weekly\" shown to the user (bar: 2 bars, values 2..5.5)"), "{s}");
        let o = run(serde_json::from_value(json!({"kind":"spark","values":[1,2,3]})).unwrap());
        assert!(!o.is_error && o.text.contains("3 values, 1..3"), "{}", o.text);
    }
}
