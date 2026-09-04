//! Agent detection: priority-ordered rules per agent, matched against a
//! tab's screen text and OSC signals.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;
use wezterm_term::{Progress, Terminal as Engine};

use crate::server::anim::Anim;

/// How long an idle result must hold before the tab shows idle.
pub const IDLE_DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentKind {
    Claude,
    Codex,
    Kiro,
}

/// The states that call for a motion cue, least pressing first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Urgency {
    Working,
    Done,
    Blocked,
}

impl Urgency {
    /// The state's status and the animation a single-line entry carries
    /// for it.
    pub fn visual(self) -> (Status, Anim) {
        match self {
            Urgency::Working => (Status::Working, Anim::Shimmer),
            Urgency::Done => (Status::Done, Anim::Shimmer),
            Urgency::Blocked => (Status::Blocked, Anim::Breathe),
        }
    }
}

/// The state a tab's status text names, which the palette colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Working,
    Blocked,
    Done,
    Idle,
}

enum Source {
    Screen,
    FromPromptLine,
    LastLineAbovePrompt,
    OscTitle,
    OscProgress,
}

/// The live screen plus OSC title and progress, captured regardless of
/// where the user has scrolled.
pub struct Snapshot {
    screen: String,
    title: String,
    progress: String,
}

impl Snapshot {
    pub fn capture(engine: &Engine) -> Self {
        let screen_state = engine.screen();
        let range = screen_state.phys_range(&(0..screen_state.physical_rows as i64));
        let mut screen = String::new();
        for line in screen_state.lines_in_phys_range(range) {
            screen.push_str(line.as_str().trim_end());
            screen.push('\n');
        }
        let progress = match engine.get_progress() {
            Progress::None => "none".to_string(),
            Progress::Percentage(p) => format!("percentage:{p}"),
            Progress::Error(p) => format!("error:{p}"),
            Progress::Indeterminate => "indeterminate".to_string(),
        };
        Self {
            screen,
            title: engine.get_title().to_string(),
            progress,
        }
    }
}

#[derive(Default)]
struct Gate {
    /// Lowercase substrings, matched against the lowercased text.
    contains: Vec<&'static str>,
    regex: Vec<Regex>,
    all: Vec<Gate>,
    any: Vec<Gate>,
    not: Vec<Gate>,
}

impl Gate {
    fn matches(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|c| lower.contains(c))
            && self.regex.iter().all(|r| r.is_match(text))
            && self.all.iter().all(|g| g.matches(text, lower))
            && (self.any.is_empty() || self.any.iter().any(|g| g.matches(text, lower)))
            && !self.not.iter().any(|g| g.matches(text, lower))
    }
}

struct Rule {
    state: AgentState,
    priority: u32,
    source: Source,
    gate: Gate,
}

fn contains(needles: &[&'static str]) -> Gate {
    Gate {
        contains: needles.to_vec(),
        ..Default::default()
    }
}

fn regex(patterns: &[&str]) -> Gate {
    Gate {
        regex: patterns
            .iter()
            .map(|p| Regex::new(p).expect("valid rule regex"))
            .collect(),
        ..Default::default()
    }
}

static CLAUDE_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        Rule {
            state: AgentState::Blocked,
            priority: 900,
            source: Source::Screen,
            gate: Gate {
                any: vec![
                    contains(&["do you want to proceed?"]),
                    contains(&["would you like to proceed?"]),
                    contains(&["do you want to make this edit"]),
                    contains(&["do you want to create"]),
                ],
                ..Default::default()
            },
        },
        // A ❯-selected numbered option, the only chrome freeform prompts
        // share. The companion evidence rules out typed input after the
        // input box's own ❯.
        Rule {
            state: AgentState::Blocked,
            priority: 890,
            source: Source::Screen,
            gate: Gate {
                regex: vec![Regex::new("(?m)^\\s*❯\\s*\\d+\\.").expect("valid rule regex")],
                any: vec![
                    regex(&["(?m)^\\s*\\d+\\."]),
                    contains(&["esc to interrupt"]),
                    contains(&["esc to cancel"]),
                ],
                ..Default::default()
            },
        },
        // An MCP elicitation dialog, matched by its frame since the
        // question is freeform.
        Rule {
            state: AgentState::Blocked,
            priority: 880,
            source: Source::Screen,
            gate: Gate {
                contains: vec!["esc to cancel"],
                regex: vec![
                    Regex::new(r#"(?im)^\s*MCP server ["“].+["”] requests your input\s*$"#)
                        .expect("valid rule regex"),
                ],
                any: vec![
                    regex(&[r"(?m)^\s*❯?\s*Accept\b"]),
                    regex(&[r"(?m)^\s*❯?\s*Decline\b"]),
                ],
                ..Default::default()
            },
        },
        // The title spinner: Braille frames on older versions, half-circle
        // frames from 2.1.228 on.
        Rule {
            state: AgentState::Working,
            priority: 850,
            source: Source::OscTitle,
            gate: regex(&["^[\u{2800}-\u{28FF}\u{25D0}-\u{25D3}]"]),
        },
        Rule {
            state: AgentState::Working,
            priority: 840,
            source: Source::OscProgress,
            gate: Gate {
                any: vec![contains(&["percentage"]), contains(&["indeterminate"])],
                ..Default::default()
            },
        },
        Rule {
            state: AgentState::Working,
            priority: 800,
            source: Source::Screen,
            gate: contains(&["esc to interrupt"]),
        },
        // The turn's spinner line, which stays up when the interrupt hint
        // rotates away or the title is suppressed.
        Rule {
            state: AgentState::Working,
            priority: 790,
            source: Source::Screen,
            gate: regex(&[r"(?m)^\s*[*·✢✶✻✽]\s+\S.*…(?:\s+\(\d+[smh](?:\s|·)|\s*$)"]),
        },
        // The status line's shell count, which outlives the turn's other
        // evidence.
        Rule {
            state: AgentState::Working,
            priority: 780,
            source: Source::Screen,
            gate: regex(&[r"(?m)^\s*[⏸⏵].*·\s+[1-9]\d*\s+shells?(?:\s+·|\s*$)"]),
        },
        // The background-agent wait line. It isn't erased when the agents
        // finish, so it only counts as the last transcript line.
        Rule {
            state: AgentState::Working,
            priority: 770,
            source: Source::LastLineAbovePrompt,
            gate: regex(&[r"^\s*[*·✢✶✻✽]\s+Waiting for [1-9]\d* background agents? to finish\s*$"]),
        },
    ]
});

/// Codex signals state through its window title: a Braille spinner while
/// working, "Action Required" when blocked.
static CODEX_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        Rule {
            state: AgentState::Blocked,
            priority: 1100,
            source: Source::OscTitle,
            gate: contains(&["action required"]),
        },
        Rule {
            state: AgentState::Working,
            priority: 1050,
            source: Source::OscTitle,
            gate: regex(&["^[\u{2800}-\u{28FF}]"]),
        },
        // Catches waits the title hasn't caught up to. Scoped from the
        // prompt line down, since finished output above can quote a prompt.
        Rule {
            state: AgentState::Blocked,
            priority: 900,
            source: Source::FromPromptLine,
            gate: Gate {
                any: vec![
                    contains(&["press enter to confirm"]),
                    contains(&["enter to submit"]),
                    contains(&["allow command?"]),
                    contains(&["[y/n]"]),
                ],
                ..Default::default()
            },
        },
    ]
});

static KIRO_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        Rule {
            state: AgentState::Blocked,
            priority: 300,
            source: Source::Screen,
            gate: Gate {
                contains: vec!["requires approval"],
                any: vec![
                    contains(&["yes, single permission"]),
                    contains(&["trust, always allow"]),
                    contains(&["no (tab to edit)"]),
                    contains(&["esc to close"]),
                ],
                ..Default::default()
            },
        },
        Rule {
            state: AgentState::Blocked,
            priority: 290,
            source: Source::Screen,
            gate: Gate {
                contains: vec!["pending from subagents"],
                any: vec![contains(&["tool approval"]), contains(&["tool approvals"])],
                all: vec![Gate {
                    any: vec![
                        contains(&["approve all pending"]),
                        contains(&["configure individually"]),
                        contains(&["exit (cancel subagents)"]),
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            },
        },
        Rule {
            state: AgentState::Working,
            priority: 100,
            source: Source::Screen,
            gate: contains(&["kiro is working"]),
        },
        // A running tool's spinner line.
        Rule {
            state: AgentState::Working,
            priority: 90,
            source: Source::Screen,
            gate: Gate {
                contains: vec!["esc to cancel"],
                regex: vec![
                    Regex::new(r"(?m)^\s*[◔◑◕●]\s+\p{Alphabetic}").expect("valid rule regex"),
                ],
                ..Default::default()
            },
        },
    ]
});

/// Cuts the input area out so typed text never counts as evidence. Kiro's
/// input chrome isn't identified, so its screen passes through unchanged.
fn without_input_area(kind: AgentKind, screen: &str) -> Cow<'_, str> {
    let lines: Vec<&str> = screen.lines().collect();
    let kept = match kind {
        AgentKind::Claude => {
            let Some((top, bottom)) = prompt_box(&lines) else {
                return Cow::Borrowed(screen);
            };
            let mut kept = lines[..=top].to_vec();
            kept.extend_from_slice(&lines[bottom..]);
            kept
        }
        AgentKind::Codex => {
            let Some(prompt) = lines.iter().rposition(|line| is_prompt_line(line)) else {
                return Cow::Borrowed(screen);
            };
            let footer = lines
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .filter(|&index| index > prompt)
                .unwrap_or(prompt + 1);
            let mut kept = lines[..prompt].to_vec();
            kept.push("›");
            kept.extend_from_slice(&lines[footer..]);
            kept
        }
        AgentKind::Kiro => return Cow::Borrowed(screen),
    };
    let mut out = String::with_capacity(screen.len());
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    Cow::Owned(out)
}

/// Codex's input prompt line.
fn is_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn from_prompt_line(screen: &str) -> &str {
    let mut start = 0;
    let mut offset = 0;
    for line in screen.split_inclusive('\n') {
        if is_prompt_line(line.trim_end_matches('\n')) {
            start = offset;
        }
        offset += line.len();
    }
    &screen[start..]
}

fn last_line_above_prompt(screen: &str) -> &str {
    let lines: Vec<&str> = screen.lines().collect();
    let top = prompt_box(&lines).map_or(lines.len(), |(top, _)| top);
    lines[..top]
        .iter()
        .rev()
        .copied()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

/// Line indices of the input box's top and bottom borders.
fn prompt_box(lines: &[&str]) -> Option<(usize, usize)> {
    let mut bottom = None;
    for (index, line) in lines.iter().enumerate().rev() {
        if is_horizontal_rule(line) {
            match bottom {
                None => bottom = Some(index),
                Some(bottom) => return Some((index, bottom)),
            }
        }
    }
    None
}

/// A run of `─`, alone on the line or at least three long when an inline
/// hint follows.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let dashes = trimmed.chars().take_while(|&c| c == '─').count();
    dashes >= 3 || (dashes > 0 && dashes == trimmed.chars().count())
}

/// The state of the highest-priority matching rule, or idle when none
/// match.
pub fn evaluate(kind: AgentKind, snapshot: &Snapshot) -> AgentState {
    let rules = match kind {
        AgentKind::Claude => &CLAUDE_RULES,
        AgentKind::Codex => &CODEX_RULES,
        AgentKind::Kiro => &KIRO_RULES,
    };
    let screen = without_input_area(kind, &snapshot.screen);
    let mut best: Option<&Rule> = None;
    for rule in rules.iter() {
        let text = match rule.source {
            Source::Screen => screen.as_ref(),
            Source::FromPromptLine => from_prompt_line(&screen),
            Source::LastLineAbovePrompt => last_line_above_prompt(&screen),
            Source::OscTitle => snapshot.title.as_str(),
            Source::OscProgress => snapshot.progress.as_str(),
        };
        let lower = text.to_lowercase();
        if rule.gate.matches(text, &lower) && best.is_none_or(|b| rule.priority > b.priority) {
            best = Some(rule);
        }
    }
    best.map_or(AgentState::Idle, |r| r.state)
}

/// A tab's debounced agent state and whether the user has seen it since
/// it went idle.
pub struct Tracker {
    kind: AgentKind,
    displayed: AgentState,
    /// When `displayed` last changed, so a cancelled idle blip doesn't
    /// reset the clock.
    since: Instant,
    pending_idle: Option<Instant>,
    seen: bool,
}

impl Tracker {
    pub fn new(kind: AgentKind) -> Self {
        Self {
            kind,
            displayed: AgentState::Idle,
            since: Instant::now(),
            pending_idle: None,
            seen: true,
        }
    }

    pub fn kind(&self) -> AgentKind {
        self.kind
    }

    /// Folds in a fresh evaluation and returns the displayed state when it
    /// changes.
    pub fn observe(&mut self, raw: AgentState, now: Instant) -> Option<AgentState> {
        if raw == self.displayed {
            self.pending_idle = None;
            return None;
        }
        if raw == AgentState::Idle {
            match self.pending_idle {
                Some(since) if now.duration_since(since) >= IDLE_DEBOUNCE => {
                    self.commit_idle(now);
                    Some(AgentState::Idle)
                }
                Some(_) => None,
                None => {
                    self.pending_idle = Some(now);
                    None
                }
            }
        } else {
            self.pending_idle = None;
            self.displayed = raw;
            self.since = now;
            Some(raw)
        }
    }

    /// Commits a pending idle once its debounce has elapsed.
    pub fn tick(&mut self, now: Instant) -> Option<AgentState> {
        match self.pending_idle {
            Some(since) if now.duration_since(since) >= IDLE_DEBOUNCE => {
                self.commit_idle(now);
                Some(AgentState::Idle)
            }
            _ => None,
        }
    }

    fn commit_idle(&mut self, now: Instant) {
        self.displayed = AgentState::Idle;
        self.since = now;
        self.pending_idle = None;
        self.seen = false;
    }

    pub fn pending(&self) -> bool {
        self.pending_idle.is_some()
    }

    pub fn mark_seen(&mut self) {
        self.seen = true;
    }

    pub fn working(&self) -> bool {
        self.displayed == AgentState::Working
    }

    pub fn needs_attention(&self) -> bool {
        match self.displayed {
            AgentState::Blocked => true,
            AgentState::Idle => !self.seen,
            AgentState::Working => false,
        }
    }

    /// Whether the status text needs timer-driven redraws, for an
    /// animation or a ticking elapsed time.
    pub fn animated(&self) -> bool {
        !(self.displayed == AgentState::Idle && self.seen)
    }

    pub fn urgency(&self) -> Option<Urgency> {
        match (self.displayed, self.seen) {
            (AgentState::Working, _) => Some(Urgency::Working),
            (AgentState::Blocked, _) => Some(Urgency::Blocked),
            (AgentState::Idle, false) => Some(Urgency::Done),
            (AgentState::Idle, true) => None,
        }
    }

    pub fn visual(&self, now: Instant) -> Visual {
        let (state, status, anim) = match (self.displayed, self.seen) {
            (AgentState::Working, _) => ("working", Status::Working, Anim::Shimmer),
            (AgentState::Blocked, _) => ("blocked", Status::Blocked, Anim::Breathe),
            (AgentState::Idle, false) => ("done", Status::Done, Anim::None),
            (AgentState::Idle, true) => ("idle", Status::Idle, Anim::None),
        };
        let text = if self.animated() {
            format!("[{state} {}]", elapsed_text(now.duration_since(self.since)))
        } else {
            format!("[{state}]")
        };
        Visual { text, status, anim }
    }
}

/// Drops precision as the duration grows so the text stays narrow.
fn elapsed_text(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// A tab's status text as the tab bar draws it.
pub struct Visual {
    pub text: String,
    pub status: Status,
    pub anim: Anim,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(screen: &str, title: &str, progress: &str) -> Snapshot {
        Snapshot {
            screen: screen.into(),
            title: title.into(),
            progress: progress.into(),
        }
    }

    #[test]
    fn urgency_ranks_blocked_over_done_over_working() {
        assert!(Urgency::Blocked > Urgency::Done);
        assert!(Urgency::Done > Urgency::Working);
        assert_eq!(
            [Urgency::Working, Urgency::Blocked, Urgency::Done]
                .into_iter()
                .max(),
            Some(Urgency::Blocked)
        );
    }

    #[test]
    fn urgency_follows_displayed_state_and_seen() {
        let now = Instant::now();
        let mut tracker = Tracker::new(AgentKind::Claude);
        assert_eq!(tracker.urgency(), None);
        tracker.observe(AgentState::Working, now);
        assert_eq!(tracker.urgency(), Some(Urgency::Working));
        tracker.observe(AgentState::Blocked, now);
        assert_eq!(tracker.urgency(), Some(Urgency::Blocked));
        tracker.observe(AgentState::Idle, now);
        tracker.tick(now + IDLE_DEBOUNCE);
        assert_eq!(tracker.urgency(), Some(Urgency::Done));
        tracker.mark_seen();
        assert_eq!(tracker.urgency(), None);
    }

    #[test]
    fn no_evidence_is_idle() {
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("$ ls\nfoo bar\n", "bash", "none")),
            AgentState::Idle
        );
    }

    #[test]
    fn interrupt_hint_is_working() {
        let s = snap("✶ Herding… (esc to interrupt)\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
    }

    #[test]
    fn spinner_title_is_working() {
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("", "⠹ claude", "none")),
            AgentState::Working
        );
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("", "◐ claude", "none")),
            AgentState::Working
        );
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("", "claude", "none")),
            AgentState::Idle
        );
    }

    #[test]
    fn spinner_line_is_working() {
        let s = snap("✻ Thinking…\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
        let s = snap("✶ Herding… (12s · ↓ 450 tokens)\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
        let s = snap("Thinking…\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
        let s = snap("· some bullet… about a thing\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn background_shell_count_is_working() {
        let s = snap(
            "⏵⏵ auto mode on (shift+tab to cycle) · 2 shells\n",
            "",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
        let s = snap("⏸ plan mode · 1 shell · ctx 6%\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
        let s = snap("the script created 2 shells\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn background_agents_line_is_working() {
        let s = snap("✻ Waiting for 2 background agents to finish\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
        let s = snap("Waiting for 2 background agents to finish\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
        let s = snap(
            "● Done. Two investigations delegated.\n\n\
             ✻ Waiting for 2 background agents to finish\n\n\
             ────────────\n❯\n────────────\n  ~/src/lux ⎇ main\n",
            "",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
    }

    #[test]
    fn finished_background_agents_line_is_idle() {
        let s = snap(
            "✻ Waiting for 1 background agent to finish\n\n\
             ● Agent \"Sleep then say hi\" completed · 17s\n\n● hi\n\n\
             ────────────\n❯\n────────────\n  ~/src/lux ⎇ main\n",
            "",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
        let s = snap("✻ Waiting for 0 background agents to finish\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn progress_is_working() {
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("", "", "percentage:40")),
            AgentState::Working
        );
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("", "", "indeterminate")),
            AgentState::Working
        );
        assert_eq!(
            evaluate(AgentKind::Claude, &snap("", "", "none")),
            AgentState::Idle
        );
    }

    #[test]
    fn permission_prompt_outranks_working_evidence() {
        let s = snap(
            "Bash command…\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n(esc to interrupt)\n",
            "⠹ claude",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Blocked);
    }

    #[test]
    fn option_selector_prompt_is_blocked() {
        let s = snap(
            "Which library should we use?\n❯ 1. serde\n  2. nanoserde\n",
            "claude",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Blocked);
        let s = snap("Pick one\n❯ 1. Yes\n(esc to interrupt)\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Blocked);
        let s = snap("1. serde\n2. nanoserde\n", "claude", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn typed_numbered_input_is_not_blocked() {
        let s = snap("❯ 1. do the first thing\n", "claude", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
        let s = snap("❯ 1. Yes\n(esc to cancel)\n", "claude", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Blocked);
    }

    #[test]
    fn typed_text_in_input_box_is_not_evidence() {
        let s = snap(
            "✻ Thinking…\n\n────────────\n❯ do you want to proceed? [y/n]\n────────────\n\
             \x20 ~/src/lux ⎇ main\n",
            "",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Working);
        let s = snap(
            "● Done.\n\n────────────\n❯ esc to interrupt\n  ❯ 1. Yes\n────────────\n\
             \x20 ~/src/lux ⎇ main\n",
            "",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
        let s = snap(
            "● Done. The command printed lux-probe.\n\n✻ Crunched for 3s · done 7:22 PM\n\n\
             ────────────\n❯ Use the AskUserQuestion tool to ask me whether I prefer red or blue.\n\
             \x20 first typed line do you want to proceed? [y/n]\n────────────\n\
             \x20 Haiku 4.5 | Context: 18% used\n  ⏸ manual mode on\n",
            "✳ Claude Code",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn dialog_below_last_rule_is_blocked() {
        // One rule on screen means no box, so the dialog isn't cut out.
        let s = snap(
            "────────────\n Bash command\n\n   mkdir -p /tmp/probe\n   Create directory /tmp/probe\n\n\
             \x20Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and always allow access to /tmp\n\
             \x20  3. No\n\n Esc to cancel · Tab to amend\n",
            "",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Blocked);
    }

    #[test]
    fn mcp_elicitation_dialog_is_blocked() {
        for screen in [
            "MCP server “my-server” requests your input\n\n\
             Grant temporary access to the demo gateway for 15 minutes?\n\n\
             ❯ Accept    Decline\n\nEsc to cancel · ↑/↓ to navigate\n",
            "MCP server \"my-server\" requests your input\n\nserver-supplied message\n\n\
             ❯ Accept    Decline\n\nEsc to cancel · ↑/↓ to navigate\n",
        ] {
            let s = snap(screen, "✳ Claude Code", "none");
            assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Blocked);
        }
        let s = snap(
            "MCP server “my-server” requests your input\n\nEsc to cancel\n",
            "✳ Claude Code",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn kiro_approval_prompts_are_blocked() {
        let s = snap(
            "Shell command requires approval\n❯ 1. Yes, single permission\n  \
             2. Trust, always allow\n  3. No (tab to edit)\n",
            "kiro",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Blocked);
        let s = snap(
            "2 tool approvals pending from subagents\n❯ Approve all pending\n  \
             Configure individually\n  Exit (cancel subagents)\n",
            "kiro",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Blocked);
        let s = snap("this tool requires approval\n", "kiro", "none");
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Idle);
    }

    #[test]
    fn kiro_working_evidence_is_working() {
        let s = snap("Kiro is working on your request\n", "kiro", "none");
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Working);
        let s = snap("◔ Running shell command (esc to cancel)\n", "kiro", "none");
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Working);
        let s = snap("press esc to cancel\n", "kiro", "none");
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Idle);
        let s = snap(
            "◑ Editing file (esc to cancel)\nrequires approval\nesc to close\n",
            "kiro",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Blocked);
    }

    #[test]
    fn kiro_no_evidence_is_idle() {
        assert_eq!(
            evaluate(AgentKind::Kiro, &snap("$ ls\nfoo bar\n", "kiro", "none")),
            AgentState::Idle
        );
    }

    #[test]
    fn kiro_rules_do_not_leak_across_agents() {
        let s = snap("(esc to interrupt)\n", "⠹ claude", "none");
        assert_eq!(evaluate(AgentKind::Kiro, &s), AgentState::Idle);
        let s = snap("Kiro is working\n", "", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Idle);
    }

    #[test]
    fn codex_spinner_title_is_working() {
        let s = snap("", "⠋ Running command", "none");
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Working);
    }

    #[test]
    fn codex_action_required_title_is_blocked() {
        let s = snap("", "Action Required — approve command", "none");
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Blocked);
        let s = snap("", "⠋ Action Required", "none");
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Blocked);
    }

    #[test]
    fn codex_confirmation_prompt_is_blocked() {
        for prompt in [
            "Press Enter to confirm or Esc to cancel\n",
            "Enter to submit answer\n",
            "Allow command? \n",
            "Continue? [y/n]\n",
        ] {
            let s = snap(prompt, "codex", "none");
            assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Blocked);
        }
    }

    #[test]
    fn codex_confirmation_text_above_prompt_is_idle() {
        let s = snap(
            "• The transcript now shows [y/N] / esc, matching the real prompt.\n\n\
             ─ Worked for 4m 59s ─\n\n› Ask Codex to do anything\n",
            "codex",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Idle);
        let s = snap("Continue? [y/n]\n›\n", "codex", "none");
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Idle);
        let s = snap(
            "• Working (4s • esc to interrupt)\n› 1. Yes, proceed\n  2. No\n\
             Press enter to confirm or esc to cancel\n",
            "codex",
            "none",
        );
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Blocked);
    }

    #[test]
    fn codex_typed_text_in_composer_is_not_evidence() {
        for screen in [
            "› do you want to continue? [y/n]\n",
            "› Explain why this prompt wraps before quoting the confirmation text\n\
             \x20 [y/N] / esc and whether the docs should include it\n\n  gpt-5 default · /work\n",
            "› first paragraph\n\n  allow command? second paragraph\n\n  ? for shortcuts\n",
        ] {
            let s = snap(screen, "codex", "none");
            assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Idle);
        }
    }

    #[test]
    fn codex_plain_title_is_idle() {
        let s = snap("$ codex\n", "codex — ~/src/lux", "none");
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Idle);
        assert_eq!(
            evaluate(AgentKind::Codex, &snap("", "", "none")),
            AgentState::Idle
        );
    }

    #[test]
    fn codex_rules_do_not_leak_into_claude() {
        let s = snap("(esc to interrupt)\n", "", "none");
        assert_eq!(evaluate(AgentKind::Codex, &s), AgentState::Idle);
        let s = snap("", "Action Required", "none");
        assert_eq!(evaluate(AgentKind::Claude, &s), AgentState::Idle);
    }

    #[test]
    fn gate_semantics_hold() {
        let gate = Gate {
            contains: vec!["alpha"],
            not: vec![contains(&["veto"])],
            ..Default::default()
        };
        assert!(gate.matches("ALPHA beta", "alpha beta"));
        assert!(!gate.matches("ALPHA veto", "alpha veto"));
        let any_gate = Gate {
            any: vec![contains(&["x"]), contains(&["y"])],
            ..Default::default()
        };
        assert!(any_gate.matches("has y", "has y"));
        assert!(!any_gate.matches("has z", "has z"));
    }

    #[test]
    fn idle_transition_debounces_and_cancels() {
        let mut t = Tracker::new(AgentKind::Claude);
        let t0 = Instant::now();
        assert_eq!(
            t.observe(AgentState::Working, t0),
            Some(AgentState::Working)
        );
        assert_eq!(t.observe(AgentState::Idle, t0), None);
        assert!(t.pending());
        assert_eq!(
            t.observe(AgentState::Working, t0 + Duration::from_millis(100)),
            None
        );
        assert!(!t.pending());
        assert_eq!(t.visual(t0).text, "[working 0s]");
        assert_eq!(
            t.observe(AgentState::Idle, t0 + Duration::from_millis(200)),
            None
        );
        let committed = t0 + Duration::from_millis(200) + IDLE_DEBOUNCE;
        assert_eq!(
            t.observe(AgentState::Idle, committed),
            Some(AgentState::Idle)
        );
        assert_eq!(t.visual(committed).text, "[done 0s]");
        t.mark_seen();
        assert_eq!(t.visual(committed).text, "[idle]");
    }

    #[test]
    fn attention_covers_done_and_blocked_only() {
        let mut t = Tracker::new(AgentKind::Claude);
        let t0 = Instant::now();
        assert!(!t.needs_attention());
        t.observe(AgentState::Working, t0);
        assert!(!t.needs_attention());
        t.observe(AgentState::Blocked, t0);
        assert!(t.needs_attention());
        t.observe(AgentState::Idle, t0);
        t.observe(AgentState::Idle, t0 + IDLE_DEBOUNCE);
        assert!(t.needs_attention());
        t.mark_seen();
        assert!(!t.needs_attention());
    }

    #[test]
    fn tick_commits_a_quiet_pending_idle() {
        let mut t = Tracker::new(AgentKind::Claude);
        let t0 = Instant::now();
        t.observe(AgentState::Working, t0);
        t.observe(AgentState::Idle, t0);
        assert_eq!(t.tick(t0 + Duration::from_millis(100)), None);
        assert_eq!(t.tick(t0 + IDLE_DEBOUNCE), Some(AgentState::Idle));
        assert_eq!(t.visual(t0 + IDLE_DEBOUNCE).text, "[done 0s]");
    }

    #[test]
    fn elapsed_text_degrades_precision_with_age() {
        assert_eq!(elapsed_text(Duration::from_secs(0)), "0s");
        assert_eq!(elapsed_text(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed_text(Duration::from_secs(60)), "1m0s");
        assert_eq!(elapsed_text(Duration::from_secs(252)), "4m12s");
        assert_eq!(elapsed_text(Duration::from_secs(3599)), "59m59s");
        assert_eq!(elapsed_text(Duration::from_secs(3600)), "1h0m");
        assert_eq!(elapsed_text(Duration::from_secs(3840)), "1h4m");
    }

    #[test]
    fn state_clock_measures_the_displayed_state() {
        let mut t = Tracker::new(AgentKind::Claude);
        let t0 = Instant::now();
        t.observe(AgentState::Working, t0);
        t.observe(AgentState::Idle, t0 + Duration::from_secs(10));
        t.observe(AgentState::Working, t0 + Duration::from_secs(11));
        assert_eq!(t.visual(t0 + Duration::from_secs(30)).text, "[working 30s]");
        t.observe(AgentState::Blocked, t0 + Duration::from_secs(40));
        assert_eq!(t.visual(t0 + Duration::from_secs(45)).text, "[blocked 5s]");
        assert!(t.animated());
        t.observe(AgentState::Idle, t0 + Duration::from_secs(50));
        t.tick(t0 + Duration::from_secs(50) + IDLE_DEBOUNCE);
        assert!(t.animated());
        t.mark_seen();
        assert!(!t.animated());
    }
}
