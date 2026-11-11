//! `/schedule`: a prompt sent after a delay, once no turn is running.

use super::*;
use mu_base::config::parse_duration;
use mu_base::events::wait_text;

/// One prompt waiting for its deadline, and then for the turn to end.
pub(super) struct Entry {
    id: u64,
    due: Instant,
    text: String,
    /// The deadline passed while a turn ran: delivered when it ends.
    fired: bool,
}

/// The scheduled prompts, earliest deadline first.
#[derive(Default)]
pub(super) struct Schedule {
    entries: Vec<Entry>,
    next_id: u64,
}

impl Schedule {
    /// Adds a prompt due at `due`; the id identifies the timer behind it.
    pub(super) fn add(&mut self, due: Instant, text: String) -> u64 {
        self.next_id += 1;
        let at = self.entries.partition_point(|e| e.due <= due);
        self.entries.insert(at, Entry { id: self.next_id, due, text, fired: false });
        self.next_id
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The pending entry with this id; `None` once it was cancelled or delivered.
    pub(super) fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub(super) fn mark_fired(&mut self, id: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.fired = true;
        }
    }

    /// The ids of the prompts whose deadline passed while a turn ran, oldest first.
    pub(super) fn fired(&self) -> Vec<u64> {
        self.entries.iter().filter(|e| e.fired).map(|e| e.id).collect()
    }

    pub(super) fn remove(&mut self, id: u64) -> Option<Entry> {
        let at = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(at))
    }

    /// Drops every entry; the number dropped.
    pub(super) fn clear(&mut self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        n
    }

    /// Drops the entry at a 1-based position in the pending list, as `/schedule list` numbers them.
    fn remove_at(&mut self, n: usize) -> Option<Entry> {
        let at = n.checked_sub(1)?;
        (at < self.entries.len()).then(|| self.entries.remove(at))
    }

    /// One line per pending prompt, oldest deadline first.
    fn rows(&self) -> Vec<String> {
        self.entries.iter().enumerate().map(|(i, e)| e.row(i + 1)).collect()
    }
}

impl Entry {
    /// `4 · in 9m 30s · the prompt`, or `4 · waiting for the turn to end · the prompt`.
    fn row(&self, n: usize) -> String {
        let when = if self.fired {
            "waiting for the turn to end".to_string()
        } else {
            format!("in {}", countdown(self.due.saturating_duration_since(Instant::now())))
        };
        format!("{n} · {when} · {}", self.text.replace('\n', " "))
    }
}

/// A wait as the two-unit text of `wait_text`, rounded up so a fresh `10m` reads `10m` and not `9m 59s`.
fn countdown(d: Duration) -> String {
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    wait_text(Duration::from_secs(secs))
}

impl App {
    /// Whether a prompt is waiting, so the countdown redraws.
    pub(super) fn schedule_pending(&self) -> bool {
        !self.schedule.is_empty()
    }

    /// `/schedule <delay> <prompt>`, `/schedule list`, `/schedule cancel [N|all]`.
    pub(super) fn schedule_cmd(&mut self, arg: &str) {
        let arg = arg.trim();
        let (verb, rest) = arg.split_once(char::is_whitespace).map_or((arg, ""), |(a, b)| (a, b.trim()));
        match verb {
            "" | "list" => self.list_schedule(),
            "cancel" => self.cancel_schedule(rest),
            delay => self.add_schedule(delay, rest),
        }
    }

    fn add_schedule(&mut self, delay: &str, text: &str) {
        let usage =
            "usage: /schedule <delay> <prompt> | list | cancel [N|all]; the delay is a duration such as 10m or 1h30m";
        let Some(d) = parse_duration(delay).filter(|d| *d > Duration::ZERO) else {
            return self.error(&format!("{usage}: {delay:?} is not one"));
        };
        let Some(due) = Instant::now().checked_add(d) else {
            return self.error("usage: /schedule <delay> <prompt>: the delay is too long");
        };
        if text.is_empty() {
            return self.error("usage: /schedule <delay> <prompt>: the prompt to send is missing");
        }
        let id = self.schedule.add(due, text.to_string());
        self.wake_at(id, due);
        self.notice(&format!("scheduled in {}: {text}", wait_text(d)));
    }

    /// The timer behind one entry: it only wakes the event loop, which owns the decision.
    fn wake_at(&self, id: u64, due: Instant) {
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep_until(due.into()).await;
            let _ = tx.send(UiMsg::ScheduleDue(id));
        });
    }

    /// The deadline passed: the prompt goes out now, or when the running turn ends.
    pub(super) fn on_schedule_due(&mut self, id: u64) {
        if self.schedule.get(id).is_none() {
            return; // cancelled: its timer still fires
        }
        self.schedule.mark_fired(id);
        if self.agent.running {
            let first = self.schedule.get(id).and_then(|e| e.text.lines().next()).unwrap_or_default().to_string();
            return self.notice(&format!("scheduled prompt due; sent when the turn ends: {first}"));
        }
        self.deliver_scheduled();
    }

    /// Sends the prompts that came due while a turn ran. Nothing runs now, so they go out in order.
    pub(super) fn deliver_scheduled(&mut self) {
        if self.agent.running {
            return;
        }
        for id in self.schedule.fired() {
            if let Some(e) = self.schedule.remove(id) {
                self.deliver_schedule(e);
            }
        }
    }

    fn deliver_schedule(&mut self, e: Entry) {
        if self.agent.status.model_id.is_empty() {
            return self.error(
                "scheduled prompt dropped: no model chosen: sign in with /login if you have not, then pick one with /switch",
            );
        }
        // A follow-up only when a turn already runs: another due prompt started one just now, and
        // the text is answered at the end of that pass rather than interrupting it.
        self.send_prompt(UserInput::from(e.text), true);
    }

    fn list_schedule(&mut self) {
        let rows = self.schedule.rows();
        if rows.is_empty() {
            return self.notice("no scheduled prompts");
        }
        self.notice(&rows.join("\n"));
    }

    fn cancel_schedule(&mut self, arg: &str) {
        match arg {
            "" | "all" => match self.schedule.clear() {
                0 => self.notice("no scheduled prompts"),
                n => self.notice(&format!("cancelled {n} scheduled prompt(s)")),
            },
            n => match n.parse::<usize>().ok().and_then(|n| self.schedule.remove_at(n)) {
                Some(e) => self.notice(&format!("cancelled: {}", e.text.lines().next().unwrap_or_default())),
                None => self.error(&format!("usage: /schedule cancel [N|all]; {n:?} is not a pending prompt")),
            },
        }
    }

    /// The pending prompts and their countdown, above the composer.
    pub(super) fn schedule_bar(&self, w: usize) -> Vec<Line> {
        let mut out: Vec<Line> = self
            .schedule
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| styled(trunc(&format!("⏱ {}", e.row(i + 1)), w), |_| dim()))
            .collect();
        if let Some(last) = out.last_mut() {
            let used: usize = line_text(last).chars().count();
            let hint = "  /schedule cancel";
            if used + hint.len() <= w {
                last.push(span(hint, dim()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use super::*;
    use mu_base::events::Command;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn history() -> PathBuf {
        std::env::temp_dir().join(format!("mu-schedule-hist-{}", std::process::id())).join("history.jsonl")
    }

    /// An app with a model chosen (a delivered prompt needs one) and its command receiver.
    fn app() -> (App, UnboundedReceiver<Command>) {
        let (mut app, cmds) = test_app_with_commands(history());
        let (ui_tx, _ui_rx) = unbounded_channel();
        app.ui_tx = ui_tx;
        app.agent.status.model_id = "anthropic/claude".into();
        (app, cmds)
    }

    fn bar(app: &App) -> String {
        app.schedule_bar(120).iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn a_schedule_is_kept_with_its_delay_and_listed() {
        in_local(async {
            let (mut app, _cmds) = app();
            app.schedule_cmd("1h30m check the build");
            assert_eq!(bar(&app), "⏱ 1 · in 1h 30m · check the build  /schedule cancel");
            assert!(transcript_text(&app).contains("scheduled in 1h 30m: check the build"));
            app.schedule_cmd("list");
            assert!(transcript_text(&app).contains("1 · in 1h 30m · check the build"));
        });
    }

    #[test]
    fn a_bad_delay_or_a_missing_prompt_is_refused() {
        in_local(async {
            let (mut app, _cmds) = app();
            app.schedule_cmd("soon check the build");
            app.schedule_cmd("10m");
            app.schedule_cmd("0s now");
            assert!(app.schedule.is_empty());
            let text = transcript_text(&app);
            assert!(text.contains("is not one"), "{text}");
            assert!(text.contains("the prompt to send is missing"), "{text}");
        });
    }

    #[test]
    fn cancel_drops_one_by_number_or_all() {
        in_local(async {
            let (mut app, _cmds) = app();
            app.schedule_cmd("10m one");
            app.schedule_cmd("20m two");
            app.schedule_cmd("cancel 2");
            assert_eq!(app.schedule.rows(), ["1 · in 10m · one"]);
            app.schedule_cmd("cancel");
            assert!(app.schedule.is_empty());
            assert!(transcript_text(&app).contains("cancelled 1 scheduled prompt(s)"));
            app.schedule_cmd("cancel 3");
            assert!(transcript_text(&app).contains("is not a pending prompt"));
        });
    }

    #[test]
    fn a_due_prompt_sends_at_once_when_nothing_runs() {
        in_local(async {
            let (mut app, mut cmds) = app();
            app.schedule_cmd("10m rehearse the deploy");
            let id = app.schedule.entries[0].id;
            app.on_schedule_due(id);
            assert!(app.schedule.is_empty());
            assert!(matches!(cmds.try_recv(), Ok(Command::Prompt(i)) if i.text == "rehearse the deploy"));
            assert!(app.agent.running, "the prompt starts a turn");
        });
    }

    #[test]
    fn a_due_prompt_waits_for_the_running_turn_to_end() {
        in_local(async {
            let (mut app, mut cmds) = app();
            app.schedule_cmd("10m rehearse the deploy");
            let id = app.schedule.entries[0].id;
            app.agent.running = true;
            app.on_schedule_due(id);
            assert!(cmds.try_recv().is_err(), "a turn is running: nothing goes out");
            assert!(bar(&app).contains("waiting for the turn to end"), "{}", bar(&app));
            assert!(transcript_text(&app).contains("sent when the turn ends"));

            app.on_agent(AgentEvent::TurnEnd { aborted: false });
            assert!(app.schedule.is_empty(), "delivered at the turn's end");
            assert!(matches!(cmds.try_recv(), Ok(Command::Prompt(i)) if i.text == "rehearse the deploy"));
        });
    }

    #[test]
    fn a_cancelled_deadline_is_ignored_when_its_timer_fires() {
        in_local(async {
            let (mut app, mut cmds) = app();
            app.schedule_cmd("10m rehearse the deploy");
            let id = app.schedule.entries[0].id;
            app.schedule_cmd("cancel");
            app.on_schedule_due(id);
            assert!(cmds.try_recv().is_err());
            assert!(app.schedule.is_empty());
        });
    }

    #[test]
    fn a_due_prompt_is_dropped_when_no_model_is_chosen() {
        in_local(async {
            let (mut app, mut cmds) = app();
            app.agent.status.model_id.clear();
            app.schedule_cmd("10m rehearse the deploy");
            let id = app.schedule.entries[0].id;
            app.on_schedule_due(id);
            assert!(cmds.try_recv().is_err());
            assert!(transcript_text(&app).contains("scheduled prompt dropped: no model chosen"));
        });
    }

    #[test]
    fn quitting_drops_pending_prompts_with_a_notice() {
        in_local(async {
            let (mut app, _cmds) = app();
            app.schedule_cmd("10m rehearse the deploy");
            app.quit();
            assert!(transcript_text(&app).contains("1 scheduled prompt(s) dropped"));
            assert!(app.schedule.is_empty());
        });
    }

    #[test]
    fn an_empty_list_says_so() {
        in_local(async {
            let (mut app, _cmds) = app();
            app.schedule_cmd("list");
            assert!(transcript_text(&app).contains("no scheduled prompts"));
        });
    }
}
