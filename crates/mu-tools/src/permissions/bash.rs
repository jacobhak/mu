//! The read-only `bash` classifier: which simple commands of a command line cannot change
//! anything. Anything it is not sure about is not read-only.

use pi_shell::bash::syntax::Features;
use pi_shell::bash::syntax::tree::{Case, Comsub, For, ProcSub};
use pi_shell::bash::syntax::{
    AndOr, ArithText, AssignValue, Command, CommandKind, Cond, CondExpr, HereDoc, List, ParamOp, Part, Pipeline,
    Program, Redir, RedirOp, RedirTarget, Sep, Seq, Simple, Subscript, Word, WordPart, parse_all,
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

/// `rg` and `tree` run only with flags from these lists: `rg --pre CMD` runs CMD and `tree -o FILE`
/// writes FILE. Plain letters take no value, valued letters take the rest of the word or the next one.
const RG_PLAIN: &str = "PFisSvwxUauLNnbHIlcoqpVh0.";
const RG_VALUED: &str = "efgmdtTABCErMj";
const RG_LONG: &[&str] = &[
    "regexp",
    "file",
    "no-search-zip",
    "engine",
    "pcre2",
    "no-pcre2",
    "encoding",
    "no-encoding",
    "crlf",
    "no-crlf",
    "no-unicode",
    "unicode",
    "fixed-strings",
    "no-fixed-strings",
    "ignore-case",
    "case-sensitive",
    "smart-case",
    "invert-match",
    "word-regexp",
    "line-regexp",
    "max-count",
    "multiline",
    "multiline-dotall",
    "no-multiline",
    "text",
    "binary",
    "no-binary",
    "unrestricted",
    "follow",
    "no-follow",
    "glob",
    "iglob",
    "glob-case-insensitive",
    "no-glob-case-insensitive",
    "hidden",
    "no-hidden",
    "no-ignore",
    "ignore",
    "ignore-file",
    "no-ignore-dot",
    "ignore-dot",
    "no-ignore-exclude",
    "ignore-exclude",
    "no-ignore-files",
    "no-ignore-global",
    "ignore-global",
    "no-ignore-parent",
    "ignore-parent",
    "no-ignore-vcs",
    "ignore-vcs",
    "no-ignore-messages",
    "ignore-messages",
    "no-require-git",
    "require-git",
    "one-file-system",
    "no-one-file-system",
    "max-depth",
    "maxdepth",
    "max-filesize",
    "mmap",
    "no-mmap",
    "type",
    "type-not",
    "type-add",
    "type-clear",
    "type-list",
    "after-context",
    "before-context",
    "context",
    "context-separator",
    "no-context-separator",
    "field-context-separator",
    "field-match-separator",
    "line-number",
    "no-line-number",
    "column",
    "no-column",
    "byte-offset",
    "with-filename",
    "no-filename",
    "files-with-matches",
    "files-without-match",
    "count",
    "count-matches",
    "include-zero",
    "only-matching",
    "replace",
    "json",
    "quiet",
    "vimgrep",
    "null",
    "null-data",
    "line-buffered",
    "no-line-buffered",
    "block-buffered",
    "no-block-buffered",
    "stop-on-nonmatch",
    "files",
    "no-messages",
    "messages",
    "sort",
    "sortr",
    "sort-files",
    "no-sort-files",
    "passthru",
    "trim",
    "no-trim",
    "max-columns",
    "max-columns-preview",
    "no-max-columns-preview",
    "color",
    "colors",
    "heading",
    "no-heading",
    "pretty",
    "stats",
    "no-stats",
    "no-config",
    "threads",
    "path-separator",
    "dfa-size-limit",
    "regex-size-limit",
    "help",
    "version",
    "pcre2-version",
];

const TREE_PLAIN: &str = "aACdDfFghilnNpqQrsSuUvxJXt";
const TREE_VALUED: &str = "ILPTH";
const TREE_LONG: &[&str] = &[
    "help",
    "version",
    "inodes",
    "device",
    "noreport",
    "nolinks",
    "dirsfirst",
    "filesfirst",
    "du",
    "si",
    "prune",
    "matchdirs",
    "ignore-case",
    "gitignore",
    "charset",
    "timefmt",
    "sort",
    "filelimit",
];

/// The flags that name a file a program reads: `--file=F`, `grep -fF`, `date -f F`.
const FILE_LONG: &[&str] = &["file", "files0-from", "from-file", "exclude-from", "ignore-file"];

/// One simple command of the line.
pub struct SimpleCmd {
    /// Its words as written, joined by single spaces, without leading assignments.
    pub text: String,
    pub argv0: Option<String>,
    /// The second word as written, for grant patterns such as `cargo test *`.
    pub second: Option<String>,
    pub assigns: bool,
    pub readonly: bool,
    /// Its words as written, one per entry of `text`; a grant can replace one to widen itself.
    pub words: Vec<String>,
    /// Its words with quotes taken off: what the shell passes on, where that is known.
    pub plain: Vec<String>,
    /// The other ways the same command reads once wrappers and `sh -c`/`eval` strings are taken off
    /// (`env git push` also reads `git push`). `deny` and `ask` rules match these too.
    pub forms: Vec<String>,
    /// The files a read-only command may open: its path words and input redirections.
    pub paths: Vec<PathWord>,
    /// The files its output redirections (and those of the groups around it) may write.
    pub writes: Vec<WriteWord>,
}

/// The target of an output redirection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteWord {
    /// As written.
    pub text: String,
    /// The file it names when the word can only mean itself: no expansion, glob or tilde.
    pub path: Option<String>,
}

/// A word that may name a file a read-only command opens. Only its text up to the first glob is
/// known, and nothing at all when an expansion could point it anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathWord {
    /// `None` for `$DIR/x` and `~/x`; otherwise the literal prefix, so `src/*.rs` is `src/`.
    pub prefix: Option<String>,
    /// Its index in `SimpleCmd.words`; `None` for an input redirection's target.
    pub word: Option<usize>,
}

/// Which arguments of a read-only program can name a file it opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Opens {
    /// No argument names a file: `echo`, `pwd`, `ps`.
    Nothing,
    /// Every positional argument does: `cat`, `ls`, `find`.
    All,
    /// All but the first positional, which is a pattern or filter: `grep`, `rg`, `jq`.
    AfterFirst,
    /// `git`: the first positional is the subcommand.
    Git,
}

pub struct BashAnalysis {
    /// The line parsed. An unparseable line has one command holding the raw text.
    pub parsed: bool,
    pub cmds: Vec<SimpleCmd>,
    /// Commands from this index on run in a directory the gate does not know: an earlier command
    /// changes it (`cd`, a word that decides the program), or a loop or an unparsed line may.
    pub unplaced_from: Option<usize>,
}

/// Programs that run the command named in their arguments.
const WRAPPERS: &[&str] =
    &["sudo", "doas", "command", "builtin", "exec", "env", "nice", "nohup", "time", "timeout", "xargs"];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh"];
/// How deep `sh -c '…'` and `eval` strings are followed.
const MAX_DEPTH: usize = 3;

/// A program word's name: quotes and a leading `\` dropped, directories stripped (`/bin/rm`).
pub(super) fn program(word: &str) -> String {
    let w = unquote(word);
    let w = w.trim_start_matches('\\');
    w.rsplit('/').next().unwrap_or(w).to_string()
}

pub(super) fn unquote(word: &str) -> String {
    word.chars().filter(|c| *c != '\'' && *c != '"').collect()
}

/// The commands `plain` may run: itself, and what its wrappers and shell strings run. Wrapper
/// options can take values (`sudo -u root rm`), so every later word that is not an option is tried
/// as the start of the command. Each form has its program word reduced to its name.
pub(super) fn unwrapped(plain: &[String], depth: usize) -> Vec<Vec<String>> {
    let Some(first) = plain.first() else { return Vec::new() };
    let mut starts = vec![0];
    if WRAPPERS.contains(&program(first).as_str()) {
        starts.extend((1..plain.len().min(32)).filter(|&i| !plain[i].starts_with('-')));
    }
    let mut out = Vec::new();
    for start in starts {
        let mut form = plain[start..].to_vec();
        let prog = program(&form[0]);
        form[0] = prog.clone();
        if depth < MAX_DEPTH {
            let script = if prog == "eval" {
                Some(form[1..].join(" "))
            } else if SHELLS.contains(&prog.as_str()) {
                form[1..]
                    .iter()
                    .position(|w| w.len() > 1 && w.starts_with('-') && !w.starts_with("--") && w.contains('c'))
                    .and_then(|i| form.get(i + 2))
                    .map(|s| unquote(s))
            } else {
                None
            };
            if let Some(script) = script {
                for c in walk(&script).cmds {
                    out.extend(unwrapped(&c.plain, depth + 1));
                }
            }
        }
        out.push(form);
    }
    out
}

/// Splits `cmd` into its simple commands and decides which of them are read-only.
pub fn analyze(cmd: &str) -> BashAnalysis {
    let mut a = walk(cmd);
    for c in &mut a.cmds {
        let mut forms: Vec<String> = Vec::new();
        for f in unwrapped(&c.plain, 0) {
            let form = f.join(" ");
            if form != c.text && !forms.contains(&form) {
                forms.push(form);
            }
        }
        c.forms = forms;
    }
    a
}

fn walk(cmd: &str) -> BashAnalysis {
    let src = cmd.as_bytes();
    let Ok(Program { commands }) = parse_all(src, Features::NONE) else {
        return BashAnalysis {
            parsed: false,
            cmds: vec![SimpleCmd {
                text: cmd.to_string(),
                argv0: None,
                second: None,
                assigns: false,
                readonly: false,
                words: Vec::new(),
                plain: Vec::new(),
                forms: Vec::new(),
                paths: Vec::new(),
                writes: Vec::new(),
            }],
            unplaced_from: Some(0),
        };
    };
    let mut w = Walker { src, cmds: Vec::new(), bad: false, group_writes: Vec::new(), loops: 0, unplaced: None };
    for c in &commands {
        w.list(c.list());
    }
    let Walker { mut cmds, bad, unplaced, .. } = w;
    if bad {
        for c in &mut cmds {
            c.readonly = false;
        }
    }
    BashAnalysis { parsed: true, cmds, unplaced_from: if bad { Some(0) } else { unplaced } }
}

struct Walker<'a> {
    src: &'a [u8],
    cmds: Vec<SimpleCmd>,
    /// A program-level construct makes every command in the line not read-only.
    bad: bool,
    /// Output redirection targets of the compound commands being walked.
    group_writes: Vec<WriteWord>,
    /// How many loops the walk is inside.
    loops: usize,
    unplaced: Option<usize>,
}

impl Walker<'_> {
    fn list(&mut self, l: List) {
        for item in l.items() {
            if matches!(item.sep(), Sep::Amp | Sep::Disown) {
                self.bad = true;
            }
            self.and_or(item.and_or());
        }
    }

    fn and_or(&mut self, a: AndOr) {
        for p in a.pipelines() {
            self.pipeline(p);
        }
    }

    fn pipeline(&mut self, p: Pipeline) {
        for cmd in p.stages() {
            self.command(cmd);
        }
    }

    /// Notes that the command about to be pushed may leave the directory it started in.
    fn moves(&mut self) {
        let n = if self.loops > 0 { 0 } else { self.cmds.len() + 1 };
        self.unplaced = Some(self.unplaced.map_or(n, |u| u.min(n)));
    }

    /// Walks a loop's parts: a command in it may run again after a later one has moved.
    fn looped(&mut self, f: impl FnOnce(&mut Self)) {
        self.loops += 1;
        f(self);
        self.loops -= 1;
    }

    fn command(&mut self, c: Command) {
        let kind = c.kind();
        let simple = matches!(kind, CommandKind::Simple(_));
        let mark = self.group_writes.len();
        let src = self.src;
        for r in c.redirs() {
            self.redir(r);
            if !simple && (is_output(r.op()) || writes_file(r).is_some()) {
                self.bad = true;
            }
            if !simple && let Some(w) = write_target(src, r) {
                self.group_writes.push(w);
            }
        }
        match kind {
            CommandKind::Simple(s) => {
                let words = s.words();
                let as_written = |w: Word| String::from_utf8_lossy(w.span().slice(src)).into_owned();
                let argv0 = words.first().and_then(literal);
                let readonly = simple_readonly(s, c.redirs());
                let moving = words.first().is_some()
                    && match argv0.as_deref() {
                        None | Some("cd" | "pushd" | "popd") => true,
                        Some("builtin" | "command" | "eval" | "." | "source") => !readonly,
                        Some(_) => false,
                    };
                if moving {
                    self.moves();
                }
                self.cmds.push(SimpleCmd {
                    text: words.iter().map(as_written).collect::<Vec<_>>().join(" "),
                    words: words.iter().map(as_written).collect(),
                    plain: words.iter().map(|w| literal(w).unwrap_or_else(|| unquote(&as_written(w)))).collect(),
                    forms: Vec::new(),
                    paths: argv0
                        .as_deref()
                        .map(|argv0| read_words(argv0, words.skip(1), c.redirs()))
                        .unwrap_or_default(),
                    writes: c
                        .redirs()
                        .iter()
                        .filter_map(|r| write_target(src, r))
                        .chain(self.group_writes.iter().cloned())
                        .collect(),
                    argv0,
                    second: words.get(1).map(as_written),
                    assigns: !s.assigns().is_empty(),
                    readonly,
                });
                self.simple_parts(s);
            }
            CommandKind::Brace(l) | CommandKind::Subshell(l) => self.list(l),
            CommandKind::If(i) => {
                for arm in i.arms() {
                    self.list(arm.cond);
                    self.list(arm.body);
                }
                if let Some(e) = i.else_body() {
                    self.list(e);
                }
            }
            CommandKind::While(w) => self.looped(|s| {
                s.list(w.cond());
                s.list(w.body());
            }),
            CommandKind::For(f) | CommandKind::Select(f) => self.looped(|s| s.for_parts(f)),
            CommandKind::ArithFor(a) => self.looped(|s| {
                s.arith(a.init());
                s.arith(a.cond());
                s.arith(a.step());
                s.list(a.body());
            }),
            CommandKind::Case(c) => self.case(c),
            CommandKind::Arith(a) => self.arith(a),
            CommandKind::Cond(e) => self.cond(e),
            CommandKind::FuncDef(f) => {
                self.bad = true;
                self.command(f.body());
            }
            CommandKind::Coproc(c) => {
                self.bad = true;
                self.command(c.body());
            }
            CommandKind::Repeat(r) => self.looped(|s| {
                s.word(r.count());
                s.list(r.body());
            }),
            CommandKind::Try(t) => {
                self.bad = true;
                self.list(t.body());
                self.list(t.always());
            }
            CommandKind::AnonFunc(a) => {
                self.bad = true;
                self.command(a.body());
                for w in a.args() {
                    self.word(w);
                }
            }
        }
        self.group_writes.truncate(mark);
    }

    fn simple_parts(&mut self, s: Simple) {
        for a in s.assigns() {
            if let Some(i) = a.index() {
                self.word(i);
            }
            match a.value() {
                AssignValue::Scalar(w) => self.word(w),
                AssignValue::Array(elems) => {
                    for e in elems {
                        if let Some(k) = e.key() {
                            self.word(k);
                        }
                        self.word(e.value());
                    }
                }
            }
        }
        for w in s.words() {
            self.word(w);
        }
    }

    fn for_parts(&mut self, f: For) {
        if let Some(ws) = f.words() {
            for w in ws {
                self.word(w);
            }
        }
        self.list(f.body());
    }

    fn case(&mut self, c: Case) {
        self.word(c.word());
        for arm in c.arms() {
            for p in arm.patterns() {
                self.word(p);
            }
            self.list(arm.body());
        }
    }

    fn redir(&mut self, r: Redir) {
        match r.target() {
            RedirTarget::Word(w) => self.word(w),
            RedirTarget::HereDoc(h) => self.heredoc(h),
        }
    }

    fn heredoc(&mut self, h: HereDoc) {
        let text = h.text();
        if !h.quoted() && (text.contains(&b'$') || text.contains(&b'`')) {
            self.bad = true;
        }
    }

    fn arith(&mut self, a: ArithText) {
        let src = a.src();
        if src.windows(2).any(|w| w == b"$(") || src.contains(&b'`') {
            self.bad = true;
        }
    }

    fn cond(&mut self, e: CondExpr) {
        match e.get() {
            Cond::Unary { arg, .. } => self.word(arg),
            Cond::Binary { lhs, rhs, .. } => {
                self.word(lhs);
                self.word(rhs);
            }
            Cond::Word(w) => self.word(w),
            Cond::Not(x) | Cond::Group(x) => self.cond(x),
            Cond::And(a, b) | Cond::Or(a, b) => {
                self.cond(a);
                self.cond(b);
            }
        }
    }

    fn word(&mut self, w: Word) {
        for p in w.parts() {
            self.part(p);
        }
    }

    fn part(&mut self, p: WordPart) {
        match p.get() {
            Part::Lit(_) | Part::Escaped(_) | Part::SQuote(_) | Part::AnsiC(_) | Part::DroppedQuote | Part::Zsh(_) => {}
            Part::DQuote(ps) | Part::Locale(ps) => {
                for p in ps {
                    self.part(p);
                }
            }
            Part::Param(param) => {
                if let Some(Subscript::Expr(w)) = param.index() {
                    self.word(w);
                }
                self.param_op(param.op());
            }
            Part::Comsub(c) => self.comsub(c),
            Part::Arith(a) => self.arith(a),
            Part::ProcSub(p) => self.procsub(p),
            Part::Compound(elems) => {
                for e in elems {
                    if let Some(k) = e.key() {
                        self.word(k);
                    }
                    self.word(e.value());
                }
            }
            Part::BadSubst(_) | Part::BadComsub(_) => self.bad = true,
        }
    }

    fn comsub(&mut self, c: Comsub) {
        self.list(c.body());
    }

    fn procsub(&mut self, p: ProcSub) {
        self.list(p.body());
    }

    fn param_op(&mut self, op: ParamOp) {
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

/// `>&word` duplicates a descriptor only for a number or `-` (optionally `N-`); any other word is
/// `&>word`, which writes the file.
fn is_fd_word(w: Word) -> bool {
    literal(w).is_some_and(|s| {
        let digits = s.strip_suffix('-').unwrap_or(&s);
        s == "-" || (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// The word of a redirection that may write a file; `/dev/null` and descriptor duplications write
/// none. `<>` opens its file for writing too.
fn writes_file(r: Redir) -> Option<Word> {
    let RedirTarget::Word(w) = r.target() else { return None };
    let writes = match r.op() {
        RedirOp::DupOut => !is_fd_word(w),
        op => (is_output(op) || matches!(op, RedirOp::ReadWrite)) && literal(w).as_deref() != Some("/dev/null"),
    };
    writes.then_some(w)
}

fn write_target(src: &[u8], r: Redir) -> Option<WriteWord> {
    let w = writes_file(r)?;
    Some(WriteWord { text: String::from_utf8_lossy(w.span().slice(src)).into_owned(), path: literal(w) })
}

fn redir_readonly(r: Redir) -> bool {
    let devnull = || matches!(r.target(), RedirTarget::Word(w) if literal(w).as_deref() == Some("/dev/null"));
    match r.op() {
        RedirOp::DupOut => writes_file(r).is_none(),
        RedirOp::In | RedirOp::DupIn | RedirOp::HereString => true,
        RedirOp::HereDoc { .. } => matches!(r.target(), RedirTarget::HereDoc(h) if h.quoted()),
        op if is_output(op) => devnull(),
        _ => false,
    }
}

fn simple_readonly(s: Simple, redirs: Seq<Redir>) -> bool {
    if !s.assigns().is_empty() {
        return false;
    }
    if !redirs.iter().all(redir_readonly) {
        return false;
    }
    let words = s.words();
    let Some(argv0) = words.first().and_then(literal) else { return false };
    program_readonly(&argv0, words.skip(1))
}

fn program_readonly(argv0: &str, args: Seq<Word>) -> bool {
    match argv0 {
        "rg" => return flags_allowed(args, RG_LONG, RG_PLAIN, RG_VALUED),
        "tree" => return flags_allowed(args, TREE_LONG, TREE_PLAIN, TREE_VALUED),
        _ => {}
    }
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

/// Whether every literal flag of `args` is on the program's list. A short cluster is read letter by
/// letter up to a letter that takes a value, which swallows the rest of the word. A word that is
/// not literal may be anything, which the path words already judge.
fn flags_allowed(args: Seq<Word>, long: &[&str], plain: &str, valued: &str) -> bool {
    for a in args {
        let Some(a) = literal(a) else { continue };
        if a == "--" {
            break;
        }
        if let Some(l) = a.strip_prefix("--") {
            if !long.contains(&l.split('=').next().unwrap_or(l)) {
                return false;
            }
        } else if let Some(cluster) = a.strip_prefix('-') {
            for c in cluster.chars() {
                if valued.contains(c) {
                    break;
                }
                if !plain.contains(c) {
                    return false;
                }
            }
        }
    }
    true
}

/// `arg` is `name` or an abbreviation of it that getopt_long still accepts (`--out` for `--output`).
fn long_flag(arg: &str, name: &str) -> bool {
    let Some(given) = arg.strip_prefix("--") else { return false };
    let given = given.split('=').next().unwrap_or(given);
    !given.is_empty() && name.strip_prefix("--").is_some_and(|n| n.starts_with(given))
}

/// A short-option cluster (`-no`) that holds `letter`.
fn short_flag(arg: &str, letter: char) -> bool {
    arg.strip_prefix('-').is_some_and(|c| !c.starts_with('-') && c.contains(letter))
}

fn restricted(program: &str, args: &[String]) -> bool {
    match program {
        "env" | "hostname" => args.is_empty(),
        "date" => !args.iter().any(|a| short_flag(a, 's') || long_flag(a, "--set")),
        "sort" => {
            !args.iter().any(|a| short_flag(a, 'o') || long_flag(a, "--output") || long_flag(a, "--compress-program"))
        }
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
        "grep" => !rest.iter().any(|a| short_flag(a, 'O') || long_flag(a, "--open-files-in-pager")),
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
fn literal(word: Word) -> Option<String> {
    let mut out = String::new();
    literal_parts(word.parts(), &mut out, true).then_some(out)
}

fn literal_parts(parts: Seq<WordPart>, out: &mut String, at_start: bool) -> bool {
    let mut first = at_start;
    for p in parts {
        if !literal_part(p, out, first) {
            return false;
        }
        first = false;
    }
    true
}

fn literal_part(p: WordPart, out: &mut String, at_start: bool) -> bool {
    match p.get() {
        Part::Lit(b) => {
            if b.iter().any(|c| matches!(c, b'*' | b'?' | b'[' | b'{')) || (at_start && b.first() == Some(&b'~')) {
                return false;
            }
            out.push_str(&String::from_utf8_lossy(b));
            true
        }
        Part::Escaped(b) | Part::SQuote(b) => {
            out.push_str(&String::from_utf8_lossy(b));
            true
        }
        Part::DQuote(parts) => literal_parts(parts, out, false),
        _ => false,
    }
}

/// The files one simple command may open: the path words its program reads, plus the targets of
/// its input redirections.
fn read_words(argv0: &str, args: Seq<Word>, redirs: Seq<Redir>) -> Vec<PathWord> {
    let mut words: Vec<PathWord> = redirs
        .iter()
        .filter(|r| matches!(r.op(), RedirOp::In))
        .filter_map(|r| match r.target() {
            RedirTarget::Word(w) => Some(PathWord { prefix: path_prefix(w), word: None }),
            RedirTarget::HereDoc(_) => None,
        })
        .collect();
    let mut value_of = vec![false; args.len()];
    let mut patterns_from_file = false;
    for (i, a) in args.iter().enumerate() {
        let Some((flag, patterns)) = literal(a).and_then(|t| file_flag(argv0, &t)) else { continue };
        patterns_from_file |= patterns;
        match (flag, args.get(i + 1)) {
            (Some(v), _) => words.push(PathWord { prefix: text_prefix(&v), word: None }),
            (None, Some(next)) => {
                value_of[i + 1] = true;
                words.push(PathWord { prefix: path_prefix(next), word: Some(i + 2) });
            }
            (None, None) => {}
        }
    }
    let opens = opens_of(argv0);
    if opens == Opens::Nothing {
        return words;
    }
    // A pattern read from a file leaves no pattern word: the first positional is already a path.
    let mut positional = usize::from(patterns_from_file);
    for (i, a) in args.iter().enumerate() {
        let text = literal(a);
        if value_of[i] || text.as_deref().is_some_and(|t| t.starts_with('-')) {
            continue;
        }
        positional += 1;
        match opens {
            // The first positional is a pattern or a subcommand, unless it is a path itself.
            Opens::AfterFirst | Opens::Git if positional == 1 && !looks_like_path(a) => continue,
            _ => words.push(PathWord { prefix: path_prefix(a), word: Some(i + 1) }),
        }
    }
    words
}

/// The value of a flag that names a file the program reads: `Some(value)` when it is attached to
/// the flag, `None` when it is the next word; `None` for any other word. The flag also says whether
/// it takes the place of the pattern word (`grep -f`).
fn file_flag(argv0: &str, arg: &str) -> Option<(Option<String>, bool)> {
    if let Some(long) = arg.strip_prefix("--") {
        let (name, value) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v.to_string())));
        return FILE_LONG.contains(&name).then_some((value, matches!(name, "file" | "from-file")));
    }
    if !matches!(argv0, "grep" | "egrep" | "fgrep" | "rg" | "date") {
        return None;
    }
    let cluster = arg.strip_prefix('-')?;
    let rest = &cluster[cluster.find('f')? + 1..];
    Some(((!rest.is_empty()).then(|| rest.to_string()), true))
}

/// `path_prefix` for a word known only as text.
fn text_prefix(text: &str) -> Option<String> {
    if text.starts_with('~') {
        return None;
    }
    Some(text.split(['*', '?', '[', '{']).next().unwrap_or_default().to_string())
}

/// Whether a word could name a place: it has a separator, is `.`/`..`, or starts with a tilde or
/// an expansion that might be one.
fn looks_like_path(word: Word) -> bool {
    match path_prefix(word) {
        Some(p) => p.contains('/') || p == "." || p == "..",
        None => true,
    }
}

fn opens_of(argv0: &str) -> Opens {
    match argv0 {
        "echo" | "printf" | "pwd" | "true" | "false" | "which" | "type" | "printenv" | "id" | "whoami" | "uname"
        | "ps" | "tr" | "date" | "env" | "hostname" | "command" | "basename" | "dirname" => Opens::Nothing,
        "grep" | "egrep" | "fgrep" | "rg" | "jq" | "yq" | "fd" => Opens::AfterFirst,
        "git" => Opens::Git,
        _ => Opens::All,
    }
}

/// The word's text up to its first glob, or nothing when a substitution or a leading `~` decides
/// where it points. A glob cannot leave the directory its prefix names.
fn path_prefix(word: Word) -> Option<String> {
    let mut out = String::new();
    prefix_parts(word.parts(), &mut out, true).then_some(out)
}

fn prefix_parts(parts: Seq<WordPart>, out: &mut String, at_start: bool) -> bool {
    let mut first = at_start;
    for p in parts {
        match p.get() {
            Part::Lit(b) => {
                if first && b.first() == Some(&b'~') {
                    return false;
                }
                match b.iter().position(|c| matches!(c, b'*' | b'?' | b'[' | b'{')) {
                    Some(i) => {
                        out.push_str(&String::from_utf8_lossy(&b[..i]));
                        return true;
                    }
                    None => out.push_str(&String::from_utf8_lossy(b)),
                }
            }
            Part::Escaped(b) | Part::SQuote(b) => out.push_str(&String::from_utf8_lossy(b)),
            Part::DQuote(ps) | Part::Locale(ps) => {
                if !prefix_parts(ps, out, false) {
                    return false;
                }
            }
            Part::DroppedQuote => {}
            _ => return false,
        }
        first = false;
    }
    true
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
    fn flags_that_run_or_write_are_not_read_only() {
        for cmd in [
            "rg --pre cat foo .",
            "rg --pre=cat foo .",
            "rg --hostname-bin=sh foo",
            "rg -z foo",
            "tree -o out.txt",
            "tree --output=out.txt",
            "git grep -O foo",
            "git grep --open-files-in-pager=sh foo",
            "git grep -nO foo",
            "sort --compress-program=sh README.md",
            "sort --compress=sh README.md",
            "sort --out=x README.md",
            "sort -no x README.md",
            "date --set=2020-01-01",
        ] {
            assert!(!ro(cmd), "{cmd}");
        }
    }

    #[test]
    fn rg_and_tree_run_with_their_listed_flags() {
        for cmd in [
            "rg -n --glob '*.rs' -t rust foo src",
            "rg -inw -A3 -B 2 foo",
            "rg --files --hidden",
            "rg -e foo -e bar",
            "rg -- -foo",
            "tree -L 2 -a -I target",
            "tree -d --dirsfirst --noreport",
            "date -u +%s",
        ] {
            assert!(ro(cmd), "{cmd}");
        }
    }

    #[test]
    fn a_flag_that_names_a_file_is_a_path_word() {
        assert_eq!(paths("wc --files0-from=/etc/shadow"), [some("/etc/shadow")]);
        assert_eq!(paths("wc --files0-from /etc/shadow"), [some("/etc/shadow")]);
        assert_eq!(paths("grep -f/etc/shadow x README.md"), [some("/etc/shadow"), some("x"), some("README.md")]);
        assert_eq!(paths("grep -rf /etc/shadow src"), [some("/etc/shadow"), some("src")]);
        assert_eq!(paths("grep --file=/etc/shadow x"), [some("/etc/shadow"), some("x")]);
        assert_eq!(paths("rg -f /etc/shadow ."), [some("/etc/shadow"), some(".")]);
        assert_eq!(paths("date -f /etc/shadow"), [some("/etc/shadow")]);
        assert_eq!(paths("date --file=~/x"), [None]);
        assert_eq!(paths("sort --files0-from=list"), [some("list")]);
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

    /// The path words of every simple command, flattened.
    fn paths(cmd: &str) -> Vec<Option<String>> {
        let a = analyze(cmd);
        assert!(a.parsed, "{cmd}");
        a.cmds.iter().flat_map(|c| c.paths.iter().map(|w| w.prefix.clone())).collect()
    }

    fn some(p: &str) -> Option<String> {
        Some(p.to_string())
    }

    #[test]
    fn commands_after_one_that_moves_are_not_placed() {
        assert_eq!(analyze("cat a; cd b; cat c").unplaced_from, Some(2));
        assert_eq!(analyze("cat a && ls").unplaced_from, None);
        assert_eq!(analyze("for i in 1 2; do cat ../x; cd ..; done").unplaced_from, Some(0), "a loop comes round");
        assert_eq!(analyze("cat a; $PROG b; cat c").unplaced_from, Some(2), "a program word decides");
        assert_eq!(analyze("command -v rg; cat c").unplaced_from, None);
        assert_eq!(analyze("eval cd ..; cat c").unplaced_from, Some(1));
        assert_eq!(analyze("ls \"unterminated").unplaced_from, Some(0));
        assert_eq!(analyze("FOO=1; cat c").unplaced_from, None, "an assignment alone moves nothing");
    }

    #[test]
    fn a_file_redirect_is_a_write_target() {
        let w = |cmd: &str| -> Vec<(String, Option<String>)> {
            analyze(cmd).cmds.iter().flat_map(|c| c.writes.iter().map(|w| (w.text.clone(), w.path.clone()))).collect()
        };
        let some = |t: &str, p: Option<&str>| (t.to_string(), p.map(String::from));
        assert_eq!(w("echo x > out.txt"), [some("out.txt", Some("out.txt"))]);
        assert_eq!(w("echo x > \"a b\""), [some("\"a b\"", Some("a b"))]);
        assert_eq!(w("echo x > $OUT"), [some("$OUT", None)]);
        assert_eq!(w("echo x <> f"), [some("f", Some("f"))], "read-write creates the file");
        assert_eq!(w("{ echo x; } >&f"), [some("f", Some("f"))]);
        assert!(w("echo x > /dev/null 2>&1").is_empty());
        assert!(!analyze("echo x <> f").cmds[0].readonly);
        assert!(!analyze("{ echo x; } <> f").cmds[0].readonly, "a group with a write is no longer read-only");
    }

    #[test]
    fn path_words_name_the_files_a_read_only_command_opens() {
        assert_eq!(paths("cat /etc/passwd"), [some("/etc/passwd")]);
        assert_eq!(paths("ls -la"), [], "no argument, so the shell's directory");
        assert_eq!(paths("ls .. && cat notes.txt"), [some(".."), some("notes.txt")]);
        assert_eq!(paths("find . -name '*.rs'"), [some("."), some("*.rs")]);
    }

    #[test]
    fn a_glob_is_judged_from_its_prefix_and_an_expansion_from_nothing() {
        assert_eq!(paths("cat src/*.rs"), [some("src/")], "a glob cannot leave its prefix");
        assert_eq!(paths("cat \"src/*.rs\""), [some("src/")]);
        assert_eq!(paths("cat $DIR/x"), [None], "a variable can point anywhere");
        assert_eq!(paths("cat $(pwd)/x"), [None], "the substitution is its own command");
        assert_eq!(paths("cat ~/.ssh/id_rsa"), [None], "so can a tilde");
        assert_eq!(paths("cat ~jacob/x"), [None]);
    }

    #[test]
    fn only_the_arguments_a_program_reads_are_path_words() {
        assert_eq!(paths("echo /etc/passwd"), [], "echo prints its arguments");
        assert_eq!(paths("pwd"), []);
        assert_eq!(paths("ps aux"), [], "ps reads /proc, whatever it is given");
        assert_eq!(paths("grep -r foo src"), [some("src")], "the first positional is the pattern");
        assert_eq!(paths("grep -f /etc/patterns src"), [some("/etc/patterns"), some("src")]);
        assert_eq!(paths("git status"), [], "the subcommand is not a path");
        assert_eq!(paths("git diff -- /etc"), [some("/etc")]);
        assert_eq!(paths("cd /etc"), [some("/etc")]);
    }

    #[test]
    fn input_redirections_are_path_words_too() {
        assert_eq!(paths("cat < /etc/passwd"), [some("/etc/passwd")]);
        assert_eq!(paths("grep x < /etc/passwd"), [some("/etc/passwd")]);
        assert_eq!(paths("cat <<'EOF'\n/etc\ny\nEOF"), [], "a heredoc is data, not a file");
        assert_eq!(paths("cat <<< /etc"), [], "so is a here string");
    }
}
