//! What the binary adds to `mu://`: its version and the command-line reference, generated from the
//! same clap definitions `mu --help` prints.

use super::Cli;
use clap::CommandFactory;
use mu_tools::tools::docs;

pub fn register() -> anyhow::Result<()> {
    docs::set_version(env!("MU_VERSION")).map_err(anyhow::Error::msg)?;
    docs::register("reference/cli.md", cli_reference);
    docs::register("reference/interactive.md", mu_tui::app::interactive_reference);
    Ok(())
}

fn cli_reference() -> String {
    let mut cmd = Cli::command().bin_name("mu").term_width(100).max_term_width(100);
    cmd.build();
    let mut s = String::from("# Command line\n\n");
    section(&mut cmd, "mu", &mut s);
    s
}

fn section(cmd: &mut clap::Command, path: &str, out: &mut String) {
    out.push_str(&format!("## `{path}`\n\n```text\n{}\n```\n\n", cmd.render_long_help().to_string().trim_end()));
    for sub in cmd.get_subcommands_mut().filter(|c| !c.is_hide_set() && c.get_name() != "help") {
        let path = format!("{path} {}", sub.get_name());
        section(sub, &path, out);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reference_covers_subcommands() {
        let s = super::cli_reference();
        assert!(s.contains("## `mu`") && s.contains("## `mu auth`") && s.contains("## `mu plan"), "{s}");
        assert!(!s.contains("--headless"), "hidden flags stay out");
    }
}
