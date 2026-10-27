//! The interactive TUI: a line-editing REPL over the engine.
//!
//! Deliberately NOT a full-screen application. `fleet tui` exists to replace the
//! `omp` TUI session that a desk attaches to over ssh or tmux (see
//! `omp_serve.sh`), and a scrolling REPL survives an 80-column ssh pipe, a tmux
//! pane, and a terminal that never sends SIGWINCH. A full-screen redraw would
//! need a cursor-addressing library (ratatui/crossterm) for no gain here.
//!
//! Raw mode is used ONLY while editing a line. During a run the terminal goes
//! back to cooked mode, so Ctrl-C keeps its normal meaning and a harness child
//! process sees a normal terminal. That is the honest trade: Ctrl-C abandons the
//! line at the prompt, and terminates the run while one is in flight.

use anyhow::Result;
use std::io::Read;

use crate::TuiArgs;

/// One turn of the conversation. The REPL holds a single session id so that
/// every turn appends to the same conversation — "never losing track" is a
/// property of resuming, not of a long-lived process.
struct Repl {
    session: Option<String>,
    harness: String,
    model: Option<String>,
    provider: Option<String>,
    unison: bool,
    serial: bool,
    verbose: bool,
    lessons: bool,
    history: Vec<String>,
    history_path: std::path::PathBuf,
}

impl Repl {
    fn new(a: &TuiArgs) -> Repl {
        let history_path = crate::paths::home().join("tui_history");
        let history = std::fs::read_to_string(&history_path)
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Repl {
            session: a.resume.clone().filter(|r| r != "last"),
            harness: a.harness.clone(),
            model: a.model.clone(),
            provider: a.provider.clone(),
            unison: false,
            serial: false,
            verbose: false,
            lessons: true,
            history,
            history_path,
        }
    }

    /// Remember a line. Best-effort: a REPL that dies because it cannot write
    /// its own history file would be worse than one that forgets.
    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().map(|l| l == line) == Some(true) {
            return;
        }
        self.history.push(line.to_string());
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    fn options(&self) -> fleet::engine::RunOptions {
        fleet::engine::RunOptions {
            harness: Some(self.harness.clone()),
            unison: self.unison,
            serial: self.serial,
            resume: self.session.clone(),
            session: None,
            verbose: self.verbose,
            brief_prefix: None,
            no_lessons: !self.lessons,
            dry: false,
            direct_model: self.model.clone(),
            direct_provider: self.provider.clone(),
            direct_api_key: None,
            cwd: None,
            continue_session: false,
        }
    }
}

// ---------------------------------------------------------------- raw mode

/// Terminal in raw mode for line editing, restored on drop.
///
/// ISIG is cleared on purpose: Ctrl-C must come back as a byte so the prompt can
/// abandon the line instead of killing the session.
#[cfg(unix)]
struct Raw {
    saved: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl Raw {
    fn enable() -> Option<Raw> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let saved = t;
            let mut raw = t;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_iflag &= !(libc::IXON | libc::ICRNL);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Raw { saved, active: true })
        }
    }
    fn disable(&mut self) {
        if self.active {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
            }
            self.active = false;
        }
    }
}

#[cfg(unix)]
impl Drop for Raw {
    fn drop(&mut self) {
        self.disable();
    }
}

/// What the user did with a line.
enum Edit {
    Line(String),
    Eof,
    Interrupted,
}

/// Read one line with editing. Falls back to a plain read when stdin is not a
/// terminal, so `echo "task" | fleet tui` and a cron-driven REPL both work.
fn read_line(prompt: &str, history: &[String]) -> Result<Edit> {
    use std::io::Write;
    #[cfg(unix)]
    {
        let is_tty = unsafe { libc::isatty(0) } == 1;
        if !is_tty {
            return plain_line(prompt);
        }
        let mut raw = match Raw::enable() {
            Some(r) => r,
            None => return plain_line(prompt),
        };
        let out = std::io::stdout();
        let mut buf = String::new();
        let mut cursor = 0usize; // byte index into buf
        let mut hpos = history.len(); // walking back through history

        // The prompt is redrawn from scratch on every keystroke: cheap, and it
        // keeps the cursor correct without tracking screen width.
        let redraw = |buf: &str, cursor: usize| {
            let mut o = out.lock();
            let (before, after) = buf.split_at(cursor);
            let _ = write!(o, "\r\x1b[K{prompt}{buf}");
            // Move the cursor back over the tail.
            if !after.is_empty() {
                let _ = write!(o, "\x1b[{}D", after.chars().count());
            }
            let _ = o.flush();
            let _ = (before, after);
        };

        redraw(&buf, cursor);
        loop {
            let mut b = [0u8; 1];
            match std::io::stdin().read(&mut b) {
                Ok(0) => {
                    let mut o = out.lock();
                    let _ = writeln!(o);
                    return Ok(Edit::Eof);
                }
                Ok(_) => {}
                Err(_) => return Ok(Edit::Eof),
            }
            match b[0] {
                b'\r' | b'\n' => {
                    let mut o = out.lock();
                    let _ = writeln!(o);
                    raw.disable(); // hand a cooked terminal to the run
                    return Ok(Edit::Line(buf));
                }
                0x03 => {
                    // Ctrl-C at the prompt: abandon this line, keep the session.
                    let mut o = out.lock();
                    let _ = write!(o, "^C\r\n");
                    raw.disable();
                    return Ok(Edit::Interrupted);
                }
                0x04 => {
                    // Ctrl-D on an empty line is EOF; otherwise delete forward.
                    if buf.is_empty() {
                        let mut o = out.lock();
                        let _ = writeln!(o);
                        return Ok(Edit::Eof);
                    }
                }
                0x7f | 0x08 => {
                    if cursor > 0 {
                        let prev = buf[..cursor].chars().last().map(|c| c.len_utf8()).unwrap_or(0);
                        buf.replace_range(cursor - prev..cursor, "");
                        cursor -= prev;
                        redraw(&buf, cursor);
                    }
                }
                0x01 => {
                    cursor = 0;
                    redraw(&buf, cursor);
                } // Ctrl-A
                0x05 => {
                    cursor = buf.len();
                    redraw(&buf, cursor);
                } // Ctrl-E
                0x0b => {
                    buf.truncate(cursor);
                    redraw(&buf, cursor);
                } // Ctrl-K
                0x15 => {
                    buf.clear();
                    cursor = 0;
                    redraw(&buf, cursor);
                } // Ctrl-U
                0x0c => {
                    let mut o = out.lock();
                    let _ = write!(o, "\x1b[2J\x1b[H");
                    let _ = o.flush();
                    redraw(&buf, cursor);
                } // Ctrl-L
                0x1b => {
                    // Escape sequence: arrows and friends.
                    let mut seq = [0u8; 2];
                    if std::io::stdin().read_exact(&mut seq).is_err() {
                        continue;
                    }
                    if seq[0] == b'[' {
                        match seq[1] {
                            b'A' => {
                                if hpos > 0 {
                                    hpos -= 1;
                                    buf = history[hpos].clone();
                                    cursor = buf.len();
                                    redraw(&buf, cursor);
                                }
                            }
                            b'B' => {
                                if hpos + 1 < history.len() {
                                    hpos += 1;
                                    buf = history[hpos].clone();
                                } else {
                                    hpos = history.len();
                                    buf.clear();
                                }
                                cursor = buf.len();
                                redraw(&buf, cursor);
                            }
                            b'C' => {
                                if cursor < buf.len() {
                                    cursor += buf[cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                                    redraw(&buf, cursor);
                                }
                            }
                            b'D' => {
                                if cursor > 0 {
                                    cursor -= buf[..cursor].chars().last().map(|c| c.len_utf8()).unwrap_or(1);
                                    redraw(&buf, cursor);
                                }
                            }
                            b'H' => {
                                cursor = 0;
                                redraw(&buf, cursor);
                            }
                            b'F' => {
                                cursor = buf.len();
                                redraw(&buf, cursor);
                            }
                            _ => {}
                        }
                    }
                }
                0x20..=0x7e | 0x80..=0xff => {
                    // Insert at the cursor. A UTF-8 lead byte (0x80..) starts a
                    // multi-byte char: read the continuation bytes before drawing.
                    let mut ch = String::new();
                    if b[0] < 0x80 {
                        ch.push(b[0] as char);
                    } else {
                        let need = if b[0] >> 5 == 0b110 {
                            1
                        } else if b[0] >> 4 == 0b1110 {
                            2
                        } else {
                            3
                        };
                        let mut bytes = vec![b[0]];
                        for _ in 0..need {
                            let mut c = [0u8; 1];
                            if std::io::stdin().read(&mut c).is_ok() {
                                bytes.push(c[0]);
                            }
                        }
                        ch = String::from_utf8_lossy(&bytes).into_owned();
                    }
                    buf.insert_str(cursor, &ch);
                    cursor += ch.len();
                    redraw(&buf, cursor);
                }
                _ => {}
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = history;
        plain_line(prompt)
    }
}

fn plain_line(prompt: &str) -> Result<Edit> {
    use std::io::Write;
    let mut o = std::io::stdout();
    let _ = write!(o, "{prompt}");
    let _ = o.flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) => Ok(Edit::Eof),
        Ok(_) => Ok(Edit::Line(line.trim_end_matches(['\r', '\n']).to_string())),
        Err(_) => Ok(Edit::Eof),
    }
}

// ---------------------------------------------------------------- commands

/// Returns true when the line was a command (and so must not be sent as a task).
fn command(r: &mut Repl, line: &str) -> bool {
    let (cmd, arg) = match line.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (line, ""),
    };
    let yes = |v: &str| !matches!(v, "off" | "no" | "false" | "0");
    match cmd {
        "/help" | "/?" => {
            println!(
                "\
  fleet tui — every line is a task; a line starting with / is a command.

  /harness <name|auto>   pin one harness, or let the planner choose
  /model <id>            call a model directly, with no agent CLI in between
  /provider <name>       which provider serves /model
  /unison [on|off]       every plausible harness answers; answers are merged
  /serial [on|off]       pipeline: each harness sees the previous answer
  /verbose [on|off]      stream harness output as it arrives
  /lessons [on|off]      brief the run with what the fleet learned
  /new                   start a fresh session (the current one is kept)
  /sessions              recent sessions
  /resume <id|last>      continue a session
  /harnesses             which harnesses are enabled
  /doctor                drift between declared, installed, and present
  /memory <key> [value]  read or write shared memory
  /clear                 clear the screen
  /exit, /quit           leave (Ctrl-D also works)

  Ctrl-C abandons the line at the prompt; during a run it aborts the run.
  Up/Down walk history; Ctrl-A/E start/end; Ctrl-U/K kill; Ctrl-L clears."
            );
            true
        }
        "/exit" | "/quit" | "/q" => {
            std::process::exit(0);
        }
        "/clear" => {
            print!("\x1b[2J\x1b[H");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            true
        }
        "/harness" => {
            r.harness = if arg.is_empty() {
                "auto".into()
            } else {
                arg.to_string()
            };
            println!("  harness = {}", r.harness);
            true
        }
        "/model" => {
            r.model = if arg.is_empty() || arg == "off" {
                None
            } else {
                Some(arg.to_string())
            };
            match &r.model {
                Some(m) => println!("  model = {m} (direct: no harness, no CLI)"),
                None => println!("  model = off (harnesses decide)"),
            }
            true
        }
        "/provider" => {
            r.provider = if arg.is_empty() || arg == "off" {
                None
            } else {
                Some(arg.to_string())
            };
            println!("  provider = {}", r.provider.as_deref().unwrap_or("auto"));
            true
        }
        "/unison" => {
            r.unison = yes(arg);
            r.serial = false;
            println!("  unison = {}", r.unison);
            true
        }
        "/serial" => {
            r.serial = yes(arg);
            r.unison = false;
            println!("  serial = {}", r.serial);
            true
        }
        "/verbose" => {
            r.verbose = yes(arg);
            println!("  verbose = {}", r.verbose);
            true
        }
        "/lessons" => {
            r.lessons = yes(arg);
            println!("  lessons = {}", r.lessons);
            true
        }
        "/new" => {
            r.session = None;
            println!("  new session");
            true
        }
        "/resume" => {
            r.session = if arg.is_empty() {
                None
            } else {
                Some(arg.to_string())
            };
            println!(
                "  resuming {}",
                r.session.as_deref().unwrap_or("(next task starts fresh)")
            );
            true
        }
        "/sessions" => {
            match fleet::memory::Memory::open_default().and_then(|m| m.sessions(12)) {
                Ok(s) => {
                    for x in s {
                        println!(
                            "  {:<28} {:>4} turns  {}  {}",
                            x.id,
                            x.turns,
                            x.updated,
                            fleet::runner::truncate(&x.title, 48)
                        );
                    }
                }
                Err(e) => println!("  {e}"),
            }
            true
        }
        "/harnesses" => {
            let (hs, _) = fleet::registry::Registry::default_dir().all();
            let on: Vec<String> = hs
                .iter()
                .filter(|h| h.enabled)
                .map(|h| h.name.clone())
                .collect();
            println!("  {} enabled: {}", on.len(), on.join(", "));
            let off: Vec<String> = hs
                .iter()
                .filter(|h| !h.enabled)
                .map(|h| h.name.clone())
                .collect();
            if !off.is_empty() {
                println!("  {} disabled: {}", off.len(), off.join(", "));
            }
            true
        }
        "/doctor" => {
            let report = fleet::doctor::check();
            if report.findings.is_empty() {
                println!("  no findings — declared, installed, and present agree");
            } else {
                for f in &report.findings {
                    println!("  {:?} {} — {}", f.severity, f.code, f.summary);
                }
            }
            true
        }
        "/memory" => {
            let (key, value) = match arg.split_once(char::is_whitespace) {
                Some((k, v)) => (k, Some(v.trim())),
                None => (arg, None),
            };
            if key.is_empty() {
                println!("  usage: /memory <key> [value]");
                return true;
            }
            // Memory is scoped; the TUI writes to its own scope so a casual
            // "/memory x y" cannot clobber another subsystem's key.
            match fleet::memory::Memory::open_default() {
                Ok(m) => match value {
                    Some(v) => match m.put("tui", key, v, "tui") {
                        Ok(ver) => println!("  {key} = {v} (v{ver})"),
                        Err(e) => println!("  {e}"),
                    },
                    None => match m.get("tui", key) {
                        Ok(Some(v)) => println!("  {key} = {v}"),
                        Ok(None) => println!("  {key} is not set in scope 'tui'"),
                        Err(e) => println!("  {e}"),
                    },
                },
                Err(e) => println!("  {e}"),
            }
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- entry

pub fn tui(a: TuiArgs, _json: bool) -> Result<()> {
    crate::paths::ensure_dirs()?;
    let mut r = Repl::new(&a);

    println!(
        "fleet {} — interactive. /help for commands, /exit to leave.",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(s) = &r.session {
        println!("resuming session {s}");
    }

    loop {
        let prompt = prompt_for(&r);
        match read_line(&prompt, &r.history)? {
            Edit::Eof => {
                println!("bye.");
                return Ok(());
            }
            Edit::Interrupted => continue,
            Edit::Line(line) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                r.remember(&line);
                if command(&mut r, &line) {
                    continue;
                }
                let opts = r.options();
                match fleet::engine::run(&line, &opts) {
                    Ok(res) => {
                        r.session = Some(res.session.clone());
                        println!("{}", res.answer);
                        let ok = res.calls.iter().filter(|c| c.ok).count();
                        let who = if res.winner.is_empty() {
                            "-".to_string()
                        } else {
                            res.winner.clone()
                        };
                        eprintln!(
                            "\x1b[2m  {} · {} call(s) {} ok · {}ms · session {}\x1b[0m",
                            who,
                            res.calls.len(),
                            ok,
                            res.ms,
                            res.session
                        );
                    }
                    Err(e) => eprintln!("\x1b[31mfleet: {e}\x1b[0m"),
                }
            }
        }
    }
}

/// The prompt states the mode, so a stale pin is visible rather than remembered.
fn prompt_for(r: &Repl) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(m) = &r.model {
        bits.push(m.clone());
    } else if r.harness != "auto" {
        bits.push(r.harness.clone());
    }
    if r.unison {
        bits.push("unison".into());
    }
    if r.serial {
        bits.push("serial".into());
    }
    if r.verbose {
        bits.push("verbose".into());
    }
    if bits.is_empty() {
        "\x1b[1;36mfleet\x1b[0m> ".to_string()
    } else {
        format!("\x1b[1;36mfleet\x1b[0m\x1b[2m({})\x1b[0m> ", bits.join(","))
    }
}
