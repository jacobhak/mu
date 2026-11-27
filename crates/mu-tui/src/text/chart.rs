//! Charts as `Line`s. ratatui's widgets draw into an offscreen `Buffer`; `to_lines` turns that buffer
//! into mu's own spans, so a chart is committed to scrollback or shown in a panel like any other text.
//! Only `ratatui-core` (Buffer, Style) and `ratatui-widgets` (Chart, BarChart, Sparkline) are used: no
//! backend, no terminal, no event loop.
//!
//! Charts draw ASCII, block and Braille cells only: a double-width grapheme in a buffer is followed by a
//! blank cell, which `to_lines` would print as an extra space.

use crossterm::style::{Attribute, Color as Ct, ContentStyle};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Rect};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::symbols::Marker;
use ratatui_core::text::Line as RLine;
use ratatui_core::widgets::Widget;
use ratatui_widgets::barchart::{Bar, BarChart, BarGroup};
use ratatui_widgets::chart::{Axis, Chart, Dataset, GraphType};
use ratatui_widgets::sparkline::Sparkline;

use mu_tools::tools::chart as spec;

use crate::text::line::{Line, span};
use crate::theme::{self, Role};

/// Series colours, in order; a chart with more series than this wraps around.
const CYCLE: [Role; 6] = [Role::Accent, Role::Ok, Role::Warn, Role::Err, Role::Tool, Role::Literal];

fn from_ct(c: Ct) -> Color {
    match c {
        Ct::Reset => Color::Reset,
        Ct::Black => Color::Black,
        Ct::DarkGrey => Color::DarkGray,
        Ct::Red => Color::LightRed,
        Ct::DarkRed => Color::Red,
        Ct::Green => Color::LightGreen,
        Ct::DarkGreen => Color::Green,
        Ct::Yellow => Color::LightYellow,
        Ct::DarkYellow => Color::Yellow,
        Ct::Blue => Color::LightBlue,
        Ct::DarkBlue => Color::Blue,
        Ct::Magenta => Color::LightMagenta,
        Ct::DarkMagenta => Color::Magenta,
        Ct::Cyan => Color::LightCyan,
        Ct::DarkCyan => Color::Cyan,
        Ct::White => Color::White,
        Ct::Grey => Color::Gray,
        Ct::Rgb { r, g, b } => Color::Rgb(r, g, b),
        Ct::AnsiValue(n) => Color::Indexed(n),
    }
}

fn to_ct(c: Color) -> Option<Ct> {
    Some(match c {
        Color::Reset => return None,
        Color::Black => Ct::Black,
        Color::DarkGray => Ct::DarkGrey,
        Color::LightRed => Ct::Red,
        Color::Red => Ct::DarkRed,
        Color::LightGreen => Ct::Green,
        Color::Green => Ct::DarkGreen,
        Color::LightYellow => Ct::Yellow,
        Color::Yellow => Ct::DarkYellow,
        Color::LightBlue => Ct::Blue,
        Color::Blue => Ct::DarkBlue,
        Color::LightMagenta => Ct::Magenta,
        Color::Magenta => Ct::DarkMagenta,
        Color::LightCyan => Ct::Cyan,
        Color::Cyan => Ct::DarkCyan,
        Color::White => Ct::White,
        Color::Gray => Ct::Grey,
        Color::Rgb(r, g, b) => Ct::Rgb { r, g, b },
        Color::Indexed(n) => Ct::AnsiValue(n),
    })
}

/// A ratatui style in the theme's colour for `role`.
fn role(r: Role) -> Style {
    theme::get().color(r).map_or_else(Style::default, |c| Style::default().fg(from_ct(c)))
}

fn content_style(fg: Color, bg: Color, m: Modifier) -> ContentStyle {
    let mut s = ContentStyle { foreground_color: to_ct(fg), background_color: to_ct(bg), ..ContentStyle::default() };
    for (flag, attr) in [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
        (Modifier::ITALIC, Attribute::Italic),
        (Modifier::UNDERLINED, Attribute::Underlined),
        (Modifier::REVERSED, Attribute::Reverse),
        (Modifier::CROSSED_OUT, Attribute::CrossedOut),
    ] {
        if m.contains(flag) {
            s.attributes.set(attr);
        }
    }
    s
}

/// The buffer as one `Line` per row: runs of cells with the same style become one span. Trailing
/// unstyled blanks are dropped so a row is not padded to the chart's width.
pub fn to_lines(buf: &Buffer) -> Vec<Line> {
    let a = buf.area;
    (a.top()..a.bottom())
        .map(|y| {
            let mut spans: Vec<(ContentStyle, String)> = Vec::new();
            for x in a.left()..a.right() {
                let cell = &buf[(x, y)];
                let st = content_style(cell.fg, cell.bg, cell.modifier);
                match spans.last_mut() {
                    Some((s, t)) if *s == st => t.push_str(cell.symbol()),
                    _ => spans.push((st, cell.symbol().to_string())),
                }
            }
            if let Some((s, t)) = spans.last_mut()
                && *s == ContentStyle::default()
            {
                t.truncate(t.trim_end().len());
            }
            spans.retain(|(_, t)| !t.is_empty());
            spans.into_iter().map(|(s, t)| span(t, s)).collect()
        })
        .collect()
}

/// Draws any widget into a `width` x `height` buffer and returns its rows.
pub fn draw(widget: impl Widget, width: usize, height: usize) -> Vec<Line> {
    let (w, h) = (width.clamp(1, u16::MAX as usize) as u16, height.clamp(1, u16::MAX as usize) as u16);
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    widget.render(buf.area, &mut buf);
    to_lines(&buf)
}

fn bounds(it: impl Iterator<Item = f64>) -> [f64; 2] {
    let (lo, hi) =
        it.filter(|v| v.is_finite()).fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| (l.min(v), h.max(v)));
    match (lo.is_finite(), hi.is_finite()) {
        (true, true) if hi > lo => [lo, hi],
        (true, true) => [lo - 1.0, hi + 1.0],
        _ => [0.0, 1.0],
    }
}

/// `n` evenly spaced labels from `lo` to `hi`, for an axis.
fn ticks(lo: f64, hi: f64, n: usize, fmt: impl Fn(f64) -> String) -> Vec<String> {
    let n = n.max(2);
    (0..n).map(|i| fmt(lo + (hi - lo) * i as f64 / (n - 1) as f64)).collect()
}

/// A number for an axis or bar: whole when large or whole, else a few significant digits.
fn num(v: f64) -> String {
    let a = v.abs();
    if a >= 100.0 || v == v.trunc() {
        format!("{v:.0}")
    } else {
        let places = if a >= 10.0 {
            1
        } else if a >= 1.0 {
            2
        } else {
            3
        };
        let s = format!("{v:.places$}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A line chart (or scatter, with `GraphType::Scatter`) of `series`. `x_labels` are spread along the x
/// axis; when empty, five numeric ticks are used. The y axis is labelled with its bounds and midpoint.
pub fn line_chart(
    series: &[spec::Series],
    x_labels: Vec<String>,
    width: usize,
    height: usize,
    graph: GraphType,
) -> Vec<Line> {
    let x = bounds(series.iter().flat_map(|s| s.points.iter().map(|p| p.0)));
    let y = bounds(series.iter().flat_map(|s| s.points.iter().map(|p| p.1)));
    let x_labels = if x_labels.is_empty() { ticks(x[0], x[1], 5, num) } else { x_labels };
    let sets: Vec<Dataset> = series
        .iter()
        .enumerate()
        .map(|(i, s)| {
            Dataset::default()
                .name(s.name.as_str())
                .marker(Marker::Braille)
                .graph_type(graph)
                .style(role(CYCLE[i % CYCLE.len()]))
                .data(&s.points)
        })
        .collect();
    let axis = role(Role::Muted);
    // The default hides the legend once it would take more than a third of the chart; show it always.
    let chart = Chart::new(sets)
        .hidden_legend_constraints((Constraint::Min(0), Constraint::Min(0)))
        .x_axis(Axis::default().style(axis).bounds(x).labels(x_labels.into_iter().map(RLine::from)))
        .y_axis(Axis::default().style(axis).bounds(y).labels(ticks(y[0], y[1], 3, num).into_iter().map(RLine::from)));
    draw(chart, width, height)
}

/// A time series: x is unix seconds. The x axis is labelled with the span's start, middle and end,
/// formatted `%m-%d %H:%M` (or `%H:%M` within one day), in the local zone.
pub fn time_series(series: &[spec::Series], width: usize, height: usize) -> Vec<Line> {
    let xs = || series.iter().flat_map(|s| s.points.iter().map(|p| p.0));
    let lo = xs().fold(f64::INFINITY, f64::min);
    let hi = xs().fold(f64::NEG_INFINITY, f64::max);
    let (lo, hi) = if lo.is_finite() && hi.is_finite() { (lo, hi) } else { (0.0, 0.0) };
    let pattern = if hi - lo < 86_400.0 { "%H:%M" } else { "%m-%d %H:%M" };
    let at = |t: f64| {
        jiff::Timestamp::from_second(t as i64)
            .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()).strftime(pattern).to_string())
            .unwrap_or_default()
    };
    line_chart(series, ticks(lo, hi, 3, at), width, height, GraphType::Line)
}

/// A vertical bar chart: one bar per (label, value), widths shared out across `width` columns. The
/// bars are scaled to whole numbers for ratatui and keep the real value as their text.
pub fn bar_chart(bars: &[(String, f64)], width: usize, height: usize) -> Vec<Line> {
    if bars.is_empty() {
        return draw(BarChart::default(), width, height);
    }
    let gap = 1;
    let bar_w = (width.saturating_sub(gap * (bars.len() - 1)) / bars.len()).clamp(1, u16::MAX as usize) as u16;
    let max = bars.iter().map(|b| b.1).fold(0.0, f64::max);
    let list: Vec<Bar> = bars
        .iter()
        .enumerate()
        .map(|(i, (label, v))| {
            let scaled = if max > 0.0 { (v / max * 100_000.0).round() as u64 } else { 0 };
            Bar::default()
                .label(RLine::from(label.as_str()))
                .value(scaled)
                .text_value(num(*v))
                .style(role(CYCLE[i % CYCLE.len()]))
        })
        .collect();
    let chart = BarChart::default()
        .data(BarGroup::default().bars(&list))
        .bar_width(bar_w)
        .bar_gap(gap as u16)
        .value_style(role(Role::Strong))
        .label_style(role(Role::Muted));
    draw(chart, width, height)
}

/// One row of block glyphs for the latest `width` of `data`, scaled between their lowest and highest.
pub fn sparkline(data: &[f64], width: usize) -> Line {
    let data = &data[data.len().saturating_sub(width)..];
    let lo = data.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = data.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let scaled = data.iter().map(|v| if hi > lo { ((v - lo) / (hi - lo) * 1000.0).round() as u64 } else { 500 });
    let chart = Sparkline::default().data(scaled).max(1000).style(role(Role::Accent));
    draw(chart, width, 1).into_iter().next().unwrap_or_default()
}

/// The rows of a validated chart at `width` columns.
pub fn render(c: &spec::Chart, width: usize) -> Vec<Line> {
    match c.kind {
        spec::Kind::Line => line_chart(&c.series, Vec::new(), width, c.height, GraphType::Line),
        spec::Kind::Scatter => line_chart(&c.series, Vec::new(), width, c.height, GraphType::Scatter),
        spec::Kind::Time => time_series(&c.series, width, c.height),
        spec::Kind::Bar => bar_chart(&c.bars, width, c.height),
        spec::Kind::Spark => {
            let lo = c.values.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = c.values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let note = format!(" {}‥{}", num(lo), num(hi));
            let w = width.saturating_sub(note.chars().count()).max(1);
            let mut row = sparkline(&c.values, w);
            row.push(span(note, theme::get().fg(Role::Faint)));
            vec![row]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::{line_text, line_width};

    fn text(rows: &[Line]) -> Vec<String> {
        rows.iter().map(line_text).collect()
    }

    fn series(name: &str, points: Vec<(f64, f64)>) -> spec::Series {
        spec::Series { name: name.into(), points }
    }

    #[test]
    fn a_parsed_call_renders_by_kind() {
        use serde_json::json;
        for (args, rows) in [
            (json!({"kind":"line","series":[{"name":"a","points":[[0,1],[1,3],[2,2]]}]}), 12),
            (json!({"kind":"scatter","height":6,"series":[{"points":[[0,1],[1,3]]}]}), 6),
            (json!({"kind":"time","series":[{"points":[["2025-01-01",1],["2025-01-09",4]]}]}), 12),
            (json!({"kind":"bar","height":8,"bars":[{"label":"a","value":1},{"label":"b","value":2}]}), 8),
            (json!({"kind":"spark","values":[1,5,2,8]}), 1),
        ] {
            let c = spec::parse(&args).unwrap();
            let out = render(&c, 50);
            assert_eq!(out.len(), rows, "{args}");
            assert!(out.iter().all(|r| line_width(r) <= 50), "{args}");
        }
    }

    #[test]
    fn colours_round_trip() {
        for c in
            [Ct::Red, Ct::DarkRed, Ct::Grey, Ct::DarkGrey, Ct::White, Ct::Rgb { r: 1, g: 2, b: 3 }, Ct::AnsiValue(99)]
        {
            assert_eq!(to_ct(from_ct(c)), Some(c));
        }
        assert_eq!(to_ct(Color::Reset), None);
    }

    #[test]
    fn a_buffer_becomes_styled_runs() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 2));
        buf.set_string(0, 0, "ab", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
        buf.set_string(2, 0, "cd", Style::default());
        let rows = to_lines(&buf);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2, "two runs, trailing blanks trimmed");
        assert_eq!(rows[0][0].text, "ab");
        assert_eq!(rows[0][0].style.foreground_color, Some(Ct::DarkRed));
        assert!(rows[0][0].style.attributes.has(Attribute::Bold));
        assert_eq!(rows[0][1].text, "cd");
        assert!(rows[1].is_empty());
    }

    #[test]
    fn a_line_chart_fits_its_box_and_shows_the_data() {
        let pts: Vec<(f64, f64)> = (0..40).map(|i| (i as f64, (i as f64 / 5.0).sin() * 10.0)).collect();
        let rows = line_chart(&[series("sin", pts)], vec![], 60, 12, GraphType::Line);
        assert_eq!(rows.len(), 12);
        assert!(rows.iter().all(|r| line_width(r) <= 60));
        let t = text(&rows).join("\n");
        assert!(t.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)), "braille cells drawn:\n{t}");
        assert!(t.contains("sin"), "legend names the series:\n{t}");
    }

    #[test]
    fn an_empty_or_flat_series_does_not_panic() {
        line_chart(&[], vec![], 30, 8, GraphType::Line);
        line_chart(&[series("a", vec![(1.0, 5.0), (2.0, 5.0)])], vec![], 30, 8, GraphType::Scatter);
        line_chart(&[series("a", vec![(f64::NAN, 1.0), (1.0, f64::INFINITY)])], vec![], 30, 8, GraphType::Line);
        bar_chart(&[], 30, 8);
        bar_chart(&[("a".into(), 0.0)], 30, 8);
        time_series(&[], 30, 8);
        sparkline(&[], 10);
        sparkline(&[3.0], 10);
    }

    #[test]
    fn a_time_series_labels_its_span() {
        let day = 86_400;
        let pts: Vec<(f64, f64)> = (0..10).map(|i| ((1_700_000_000 + i * day) as f64, i as f64)).collect();
        let t = text(&time_series(&[series("cost", pts)], 70, 10)).join("\n");
        assert!(t.contains("11-1"), "dates label the x axis:\n{t}");
    }

    #[test]
    fn bars_scale_and_label() {
        let bars: Vec<(String, f64)> = vec![("mon".into(), 3.0), ("tue".into(), 9.5), ("wed".into(), 6.0)];
        let rows = bar_chart(&bars, 30, 10);
        assert_eq!(rows.len(), 10);
        let t = text(&rows).join("\n");
        for l in ["mon", "tue", "wed"] {
            assert!(t.contains(l), "{l} labelled:\n{t}");
        }
        assert!(t.contains('█'), "bars drawn:\n{t}");
        assert!(t.contains("9.5"), "the real value labels the bar:\n{t}");
    }

    #[test]
    fn a_sparkline_is_one_row_of_the_latest_values() {
        let data: Vec<f64> = (0..100).map(f64::from).collect();
        let l = sparkline(&data, 20);
        assert_eq!(line_width(&l), 20);
        assert!(line_text(&l).ends_with('█'));
    }
}
