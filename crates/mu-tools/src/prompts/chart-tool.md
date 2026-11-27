Draws a chart in the user's terminal. Use it when the shape of numbers answers the question better than a table: a trend, a comparison, a distribution over time. You get back a one-line summary, not the picture, so state conclusions in your reply and do not repeat the data.
<instruction>
- `kind: "line"` or `"scatter"`: `series` of `{name, points: [[x, y], ...]}` with numeric x. `"time"`: same, but x is unix seconds or an ISO 8601 date or timestamp string (`"2025-01-31"`, `"2025-01-31T14:00:00Z"`); points are sorted by time. `"bar"`: `bars` of `{label, value}` with values of at least 0; keep labels short. `"spark"`: `values`, one compact row for a trend inside a sentence-sized answer.
- Several series share one chart and one y scale: draw series on different scales as separate charts. At most 12 series, 5000 points in all, 60 bars, 400 spark values: aggregate or downsample first.
- Set `title` when more than one chart is shown. `height` is in rows (4 to 40, default 12).
- The data must come from a tool result or the user, never from memory or a guess.
- Not for exact figures the user will copy: give those as text or a table.
</instruction>
