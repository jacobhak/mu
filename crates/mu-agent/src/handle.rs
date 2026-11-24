//! A front end's handle on a running agent: commands in, events out, and the task. `build_agent` is
//! the one place an agent is wired up.

use crate::agent::{Agent, AgentInit, DeliverFn, RunConfig};
use mu_base::events::{AgentEvent, Command};
use mu_base::message::UserInput;
use mu_base::models::{ModelSpec, Thinking};
use mu_provider::provider::client::Providers;
use mu_tools::context::Context;
use mu_tools::plugin::host::PluginHost;
use serde_json::Value;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// How often the TUI agent reads the working tree for the status line.
const GIT_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Which background features an agent turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The TUI: background jobs and `mu notify` delivery.
    Tui,
    /// ACP sessions: background jobs.
    Acp,
    /// `-p` runs and subagents: neither.
    Headless,
}

/// What `build_agent` needs besides the working directory and the mode.
pub struct AgentSetup {
    pub providers: Rc<Providers>,
    pub plugins: Rc<PluginHost>,
    pub ctx: Rc<Context>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    pub deliver: DeliverFn,
    pub run: RunConfig,
    /// Structured-output schema for `yield`.
    pub output_schema: Option<Value>,
}

pub struct AgentHandle {
    pub cmd: UnboundedSender<Command>,
    pub ev: UnboundedReceiver<AgentEvent>,
    /// Wakes the agent for `mu notify` (SIGUSR2). `Some` only in `Mode::Tui`.
    pub wake: Option<UnboundedSender<()>>,
    /// The agent task. It ends after `Command::Exit`.
    pub task: JoinHandle<()>,
}

impl AgentHandle {
    pub fn send(&self, c: Command) {
        let _ = self.cmd.send(c);
    }

    /// Queues `input` for the running turn: it is delivered between tool calls.
    pub fn steer(&self, input: impl Into<UserInput>) {
        self.send(Command::Steer(input.into()));
    }

    /// Queues `input` to run after the running turn.
    pub fn follow_up(&self, input: impl Into<UserInput>) {
        self.send(Command::FollowUp(input.into()));
    }

    /// Aborts the in-flight turn: a provider call or a tool. Esc, plan review cancel, ACP cancel, SIGUSR1.
    pub fn cancel(&self) {
        self.send(Command::Cancel);
    }

    /// Finishes at the next tool or turn boundary, leaving the session resumable (headless first Ctrl-C).
    pub fn stop(&self) {
        self.send(Command::Stop);
    }
}

/// Builds an agent for `cwd` and spawns it on the current `LocalSet`.
pub fn build_agent(s: AgentSetup, cwd: PathBuf, mode: Mode) -> AgentHandle {
    let (events, ev) = unbounded_channel();
    let (cmd, cmd_rx) = unbounded_channel();
    let mut agent = Agent::new(AgentInit {
        providers: s.providers,
        plugins: s.plugins,
        cwd,
        ctx: s.ctx,
        model: s.model,
        thinking: s.thinking,
        events,
        deliver: s.deliver,
        run: s.run,
    });
    agent.set_output_schema(s.output_schema);
    let mut wake = None;
    if mode != Mode::Headless {
        agent.enable_jobs();
    }
    if mode == Mode::Tui {
        wake = agent.enable_notify();
        agent.watch_git(GIT_POLL);
    }
    let task = tokio::task::spawn_local(agent.run(cmd_rx));
    AgentHandle { cmd, ev, wake, task }
}
