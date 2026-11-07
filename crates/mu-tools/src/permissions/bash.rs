//! The read-only `bash` classifier: which simple commands of a command line cannot change
//! anything. Anything it is not sure about is not read-only.

use pi_shell::bash::syntax::Features;
use pi_shell::bash::syntax::{
    AssignValue, Case, Command, CommandKind, Comsub, CondExpr, For, HereDoc, List, ParamOp, Pipeline, ProcSub, Program,
    Redir, RedirOp, RedirTarget, Sep, Simple, Stage, Subscript, Word, WordPart, parse_all,
};

/// Programs whose arguments are never expanded into filesystem writes or command runs: any
/// argument is fine, and every command inside their substitutions is classified on its own.
const ANY_ARGS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "pwd",
    "echo",
    "printf",
    "true",
    "false",
    "which",
    "type",
    "stat",
    "du",
    "df",
    "tree",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "diff",
    "cmp",
    "cut",
    "tr",
    "nl",
    "column",
    "jq",
    "printenv",
    "id",
    "whoami",
    "uname",
    "ps",
    "test",
    "[",
    "sha256sum",
    "sha1sum",
    "md5sum",
    "b2sum",
    "cksum",
    "od",
    "hexdump",
    "strings",
    "file",
    "cd",
];

/// `git` subcommands that only read.
const GIT_READONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "cat-file",
    "blame",
    "describe",
    "shortlog",
    "merge-base",
    "name-rev",
    "grep",
];

/// One simple command of the line.
pub struct SimpleCmd {
    /// Its words as written, joined by single spaces, without leading assignments.
    pub text: String,
    pub argv0: Option<String>,
    /// The second word as written, for grant patterns such as `cargo test *`.
    pub second: Option<String>,
    pub assigns: bool,
    pub readonly: bool,
}

pub struct BashAnalysis {
    /// The line parsed. An unparseable line has one command holding the raw text.
    pub parsed: bool,
    pub cmds: Vec<SimpleCmd>,
}

/// Splits `cmd` into its simple commands and decides which of them are read-only.
pub fn analyze(cmd: &str) -> BashAnalysis {
    let src = cmd.as_bytes();
    let Ok(Program { commands }) = parse_all(src, Features::NONE) else {
        return BashAnalysis {
            parsed: false,
            cmds: vec![SimpleCmd { text: cmd.to_string(), argv0: None, second: None, assigns: false, readonly: false }],
        };
    };
    let mut w = Walker { src, cmds: Vec::new(), bad: false };
    for c in &commands {
        w.list(&c.list);
    }
    let Walker { mut cmds, bad, .. } = w;
    if bad {
        for c in &mut cmds {
            c.readonly = false;
        }
    }
    BashAnalysis { parsed: true, cmds }
}

struct Walker<'a> {
    src: &'a [u8],
    cmds: Vec<SimpleCmd>,
    /// A program-level construct makes every command in the line not read-only.
    bad: bool,
}

impl Walker<'_> {
    fn list(&mut self, l: &List) {
        for item in &l.items {
            if matches!(item.sep, Sep::Amp | Sep::Disown) {
                self.bad = true;
            }
            self.and_or(&item.and_or);
        }
    }

    fn and_or(&mut self, a: &pi_shell::bash::syntax::AndOr) {
        self.pipeline(&a.first);
        for (_, p) in &a.rest {
            self.pipeline(p);
        }
    }

    fn pipeline(&mut self, p: &Pipeline) {
        for Stage { cmd, .. } in &p.stages {
            self.command(cmd);
        }
    }

    fn command(&mut self, c: &Command) {
        let simple = matches!(c.kind, CommandKind::Simple(_));
        for r in &c.redirs {
            self.redir(r);
            if !simple && is_output(r.op) {
                self.bad = true;
            }
        }
        match &c.kind {
            CommandKind::Simple(s) => {
                self.cmds.push(SimpleCmd {
                    text: s
                        .words
                        .iter()
                        .map(|w| String::from_utf8_lossy(w.span.slice(self.src)).into_owned())
                        .collect::<Vec<_>>()
                        .join(" "),
                    argv0: s.words.first().and_then(literal),
                    second: s.words.get(1).map(|w| String::from_utf8_lossy(w.span.slice(self.src)).into_owned()),
                    assigns: !s.assigns.is_empty(),
                    readonly: simple_readonly(s, &c.redirs),
                });
                self.simple_parts(s);
            }
            CommandKind::Brace(l) | CommandKind::Subshell(l) => self.list(l),
            CommandKind::If(i) => {
                for arm in &i.arms {
                    self.list(&arm.cond);
                    self.list(&arm.body);
                }
                if let Some(e) = &i.else_body {
                    self.list(e);
                }
            }
            CommandKind::While(w) => {
                self.list(&w.cond);
                self.list(&w.body);
            }
            CommandKind::For(f) | CommandKind::Select(f) => self.for_parts(f),
            CommandKind::ArithFor(a) => {
                self.arith(&a.init);
                self.arith(&a.cond);
                self.arith(&a.step);
                self.list(&a.body);
            }
            CommandKind::Case(c) => self.case(c),
            CommandKind::Arith(a) => self.arith(a),
            CommandKind::Cond(e) => self.cond(e),
            CommandKind::FuncDef(f) => {
                self.bad = true;
                self.command(&f.body);
            }
            CommandKind::Coproc(c) => {
                self.bad = true;
                self.command(&c.body);
            }
            CommandKind::Repeat(r) => {
                self.word(&r.count);
                self.list(&r.body);
            }
            CommandKind::Try(t) => {
                self.bad = true;
                self.list(&t.body);
                self.list(&t.always);
            }
            CommandKind::AnonFunc(a) => {
                self.bad = true;
                self.command(&a.body);
                for w in &a.args {
                    self.word(w);
                }
            }
        }
    }

    fn simple_parts(&mut self, s: &Simple) {
        for a in &s.assigns {
            if let Some(i) = &a.index {
                self.word(i);
            }
            match &a.value {
                AssignValue::Scalar(w) => self.word(w),
                AssignValue::Array(elems) => {
                    for e in elems {
                        if let Some(k) = &e.key {
                            self.word(k);
                        }
                        self.word(&e.value);
                    }
                }
            }
        }
        for w in &s.words {
            self.word(w);
        }
    }

    fn for_parts(&mut self, f: &For) {
        if let Some(ws) = &f.words {
            for w in ws {
                self.word(w);
            }
        }
        self.list(&f.body);
    }

    fn case(&mut self, c: &Case) {
        self.word(&c.word);
        for arm in &c.arms {
            for p in &arm.patterns {
                self.word(p);
            }
            self.list(&arm.body);
        }
    }

    fn redir(&mut self, r: &Redir) {
        match &r.target {
            RedirTarget::Word(w) => self.word(w),
            RedirTarget::HereDoc(h) => self.heredoc(h),
        }
    }

    fn heredoc(&mut self, h: &HereDoc) {
        if !h.quoted && (h.text.contains(&b'$') || h.text.contains(&b'`')) {
            self.bad = true;
        }
    }

    fn arith(&mut self, a: &pi_shell::bash::syntax::ArithText) {
        if a.src.windows(2).any(|w| w == b"$(") || a.src.contains(&b'`') {
            self.bad = true;
        }
    }

    fn cond(&mut self, e: &CondExpr) {
        match e {
            CondExpr::Unary { arg, .. } => self.word(arg),
            CondExpr::Binary { lhs, rhs, .. } => {
                self.word(lhs);
                self.word(rhs);
            }
            CondExpr::Word(w) => self.word(w),
            CondExpr::Not(x) | CondExpr::Group(x) => self.cond(x),
            CondExpr::And(a, b) | CondExpr::Or(a, b) => {
                self.cond(a);
                self.cond(b);
            }
        }
    }

    fn word(&mut self, w: &Word) {
        for p in &w.parts {
            self.part(p);
        }
    }

    fn part(&mut self, p: &WordPart) {
        match p {
            WordPart::Lit(_)
            | WordPart::Escaped(_)
            | WordPart::SQuote(_)
            | WordPart::AnsiC(_)
            | WordPart::DroppedQuote
            | WordPart::Zsh(_) => {}
            WordPart::DQuote(ps) | WordPart::Locale(ps) => {
                for p in ps {
                    self.part(p);
                }
            }
            WordPart::Param(param) => {
                if let Some(Subscript::Expr(w)) = &param.index {
                    self.word(w);
                }
                self.param_op(&param.op);
            }
            WordPart::Comsub(c) => self.comsub(c),
            WordPart::Arith(a) => self.arith(a),
            WordPart::ProcSub(p) => self.procsub(p),
            WordPart::Compound(elems) => {
                for e in elems {
                    if let Some(k) = &e.key {
                        self.word(k);
                    }
                    self.word(&e.value);
                }
            }
            WordPart::BadSubst(_) | WordPart::BadComsub(_) => self.bad = true,
        }
    }

    fn comsub(&mut self, c: &Comsub) {
        self.list(&c.body);
    }

    fn procsub(&mut self, p: &ProcSub) {
        self.list(&p.body);
    }

    fn param_op(&mut self, op: &ParamOp) {
        match op {
            ParamOp::Default { word, .. } | ParamOp::Assign { word, .. } | ParamOp::Alt { word, .. } => self.word(word),
            ParamOp::Error { word, .. } => {
                if let Some(w) = word {
                    self.word(w);
                }
            }
            ParamOp::Substr { offset, len } => {
                self.arith(offset);
                if let Some(l) = len {
                    self.arith(l);
                }
            }
            ParamOp::Trim { pat, .. } => self.word(pat),
            ParamOp::Replace { pat, rep, .. } => {
                self.word(pat);
                if let Some(r) = rep {
                    self.word(r);
                }
            }
            ParamOp::Case { pat, .. } => {
                if let Some(p) = pat {
                    self.word(p);
                }
            }
            ParamOp::None
            | ParamOp::Length
            | ParamOp::Indirect
            | ParamOp::PrefixNames { .. }
            | ParamOp::Transform(_) => {}
        }
    }
}

fn is_output(op: RedirOp) -> bool {
    matches!(
        op,
        RedirOp::Out
            | RedirOp::Append
            | RedirOp::Clobber
            | RedirOp::OutErr
            | RedirOp::AppendErr
            | RedirOp::AppendClobber
            | RedirOp::OutErrClobber
            | RedirOp::AppendErrClobber
    )
}

fn redir_readonly(r: &Redir) -> bool {
    let devnull = || matches!(&r.target, RedirTarget::Word(w) if literal(w).as_deref() == Some("/dev/null"));
    match r.op {
        RedirOp::In | RedirOp::DupIn | RedirOp::DupOut | RedirOp::HereString => true,
        RedirOp::HereDoc { .. } => matches!(&r.target, RedirTarget::HereDoc(h) if h.quoted),
        op if is_output(op) => devnull(),
        _ => false,
    }
}

fn simple_readonly(s: &Simple, redirs: &[Redir]) -> bool {
    if !s.assigns.is_empty() {
        return false;
    }
    if !redirs.iter().all(redir_readonly) {
        return false;
    }
    let Some(argv0) = s.words.first().and_then(literal) else { return false };
    program_readonly(&argv0, &s.words[1..])
}

fn program_readonly(argv0: &str, args: &[Word]) -> bool {
    if ANY_ARGS.contains(&argv0) {
        return true;
    }
    let mut lits = Vec::with_capacity(args.len());
    for a in args {
        match literal(a) {
            Some(s) => lits.push(s),
            None => return false,
        }
    }
    restricted(argv0, &lits)
}

fn restricted(program: &str, args: &[String]) -> bool {
    match program {
        "env" | "hostname" => args.is_empty(),
        "date" => !args.iter().any(|a| a == "-s" || a.starts_with("-s") || a.starts_with("--set")),
        "sort" => !args.iter().any(|a| a.starts_with("-o") || a.starts_with("--output")),
        "uniq" | "xxd" => args.iter().filter(|a| !a.starts_with('-')).count() <= 1,
        "yq" => !args.iter().any(|a| matches!(a.as_str(), "-i" | "--inplace" | "--in-place")),
        "fd" => !args.iter().any(|a| matches!(a.as_str(), "-x" | "-X" | "--exec" | "--exec-batch")),
        "find" => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fprint" | "-fprint0" | "-fprintf" | "-fls"
            )
        }),
        "command" => matches!(args.first().map(String::as_str), Some("-v" | "-V")),
        "git" => git_readonly(args),
        _ => false,
    }
}

fn git_readonly(args: &[String]) -> bool {
    if args.iter().any(|a| a.starts_with("--output") || a.starts_with("--ext-diff")) {
        return false;
    }
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-pager" | "-P" => i += 1,
            "-C" => {
                if i + 1 >= args.len() {
                    return false;
                }
                i += 2;
            }
            a if a.starts_with('-') => return false,
            _ => break,
        }
    }
    let Some(sub) = args.get(i).map(String::as_str) else { return false };
    let rest = &args[i + 1..];
    match sub {
        "branch" => branch_readonly(rest),
        "tag" => rest.is_empty() || matches!(rest[0].as_str(), "-l" | "--list"),
        "remote" => {
            rest.is_empty() || (rest.len() == 1 && rest[0] == "-v") || (rest.len() == 2 && rest[0] == "get-url")
        }
        "config" => {
            matches!(rest.first().map(String::as_str), Some("--get" | "--get-all" | "--get-regexp" | "--list" | "-l"))
        }
        "stash" => matches!(rest.first().map(String::as_str), Some("list" | "show")),
        "worktree" => matches!(rest.first().map(String::as_str), Some("list")),
        "reflog" => rest.is_empty() || rest[0] == "show",
        _ if GIT_READONLY.contains(&sub) => true,
        _ => false,
    }
}

fn branch_readonly(args: &[String]) -> bool {
    const FLAGS: &[&str] = &[
        "-a",
        "-r",
        "-v",
        "-vv",
        "--list",
        "--show-current",
        "--all",
        "--remotes",
        "--verbose",
        "--contains",
        "--merged",
        "--no-merged",
    ];
    const POSITIONAL_OK: &[&str] = &["--list", "--contains", "--merged", "--no-merged"];
    let mut positional = false;
    for a in args {
        if FLAGS.contains(&a.as_str()) {
            if POSITIONAL_OK.contains(&a.as_str()) {
                positional = true;
            }
        } else if a.starts_with("--format=") || a.starts_with("--sort=") {
        } else if a.starts_with('-') || !positional {
            return false;
        }
    }
    true
}

/// The text of a word when it can only mean itself: no globbing, brace expansion or tilde
/// expansion, and no substitution. Quoted parts are literal (`AnsiC` is not: it decodes).
fn literal(word: &Word) -> Option<String> {
    let mut out = String::new();
    literal_parts(&word.parts, &mut out, true).then_some(out)
}

fn literal_parts(parts: &[WordPart], out: &mut String, at_start: bool) -> bool {
    let mut first = at_start;
    for p in parts {
        if !literal_part(p, out, first) {
            return false;
        }
        first = false;
    }
    true
}

fn literal_part(p: &WordPart, out: &mut String, at_start: bool) -> bool {
    match p {
        WordPart::Lit(b) => {
            if b.iter().any(|c| matches!(c, b'*' | b'?' | b'[' | b'{')) || (at_start && b.first() == Some(&b'~')) {
                return false;
            }
            out.push_str(&String::from_utf8_lossy(b));
            true
        }
        WordPart::Escaped(b) | WordPart::SQuote(b) => {
            out.push_str(&String::from_utf8_lossy(b));
            true
        }
        WordPart::DQuote(parts) => literal_parts(parts, out, false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every simple command of `cmd` is read-only.
    fn ro(cmd: &str) -> bool {
        let a = analyze(cmd);
        a.parsed && !a.cmds.is_empty() && a.cmds.iter().all(|c| c.readonly)
    }

    #[test]
    fn read_only_commands_are_recognised() {
        for cmd in [
            "ls -la",
            "git status && git diff HEAD~1",
            "rg foo | head -5",
            "cat $(git ls-files)",
            "cd src && ls",
            "for f in *.rs; do wc -l $f; done",
            "grep x a 2>/dev/null",
            "git log --oneline -3",
            "git branch --list 'feature/*'",
            "git tag -l",
            "git remote -v",
            "git config --get user.name",
            "git stash list",
            "git worktree list",
            "git reflog show",
            "find . -name '*.rs' -type f",
            "sort -n file",
            "env",
            "date",
            "command -v rg",
            "cat <<'EOF'\n$x\nEOF",
        ] {
            assert!(ro(cmd), "{cmd}");
        }
    }

    #[test]
    fn writes_and_uncertain_shapes_are_not_read_only() {
        for cmd in [
            "ls > out",
            "git push",
            "git -c core.pager=x log",
            "git diff --output=x",
            "find . -delete",
            "sort -o x y",
            "ls &",
            "f(){ ls; }; f",
            "FOO=1 ls",
            "echo $(rm x)",
            "ls \"unterminated",
            "cat <<EOF\n$(rm x)\nEOF",
            "ls | tee out",
            "sed -i s/a/b/ f",
        ] {
            assert!(!ro(cmd), "{cmd}");
        }
    }

    #[test]
    fn assignments_are_not_coverage_and_subjects_are_as_written() {
        let a = analyze("FOO=1 ls -la");
        assert!(a.parsed);
        assert!(a.cmds[0].assigns);
        assert!(!a.cmds[0].readonly);
        assert_eq!(a.cmds[0].text, "ls -la");
        let a = analyze("cargo test --release");
        assert_eq!(a.cmds[0].argv0.as_deref(), Some("cargo"));
        assert_eq!(a.cmds[0].second.as_deref(), Some("test"));
        assert!(!a.cmds[0].readonly, "cargo is not a known read-only program");
    }

    #[test]
    fn an_unparseable_line_is_one_raw_subject() {
        let a = analyze("ls \"unterminated");
        assert!(!a.parsed);
        assert_eq!(a.cmds.len(), 1);
        assert_eq!(a.cmds[0].text, "ls \"unterminated");
        assert!(!a.cmds[0].readonly);
        assert_eq!(a.cmds[0].argv0, None);
    }
}
