//! Claude Code agent detection (Phase 9): hardcoded, priority-ordered
//! rules gated by nested `all`/`any`/`not` combinators over
//! `contains`/`regex` matchers, evaluated against a tab's visible screen
//! text and OSC title/progress signals. The configurable TOML delivery
//! mechanism is deliberately absent.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ratatui::style::Color;
use regex::Regex;
use wezterm_term::{Progress, Terminal as Engine};

use crate::server::anim::Anim;

/// How long a working/blocked → idle result must hold
/// before the displayed state updates.
pub const IDLE_DEBOUNCE: Duration = Duration::from_millis(400);

/// The three detectable states.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
}

/// The evidence a rule matches against: the tab's
/// current screen text, or the OSC title / OSC 9;4 progress state the
/// engine captured from the PTY stream.
enum Source {
    Screen,
    OscTitle,
    OscProgress,
}

/// One evaluation snapshot, taken from the live screen bottom regardless
/// of where the user has scrolled.
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
        // OSC 0/2 title and OSC 9;4 progress, as captured
        // by the engine while parsing the PTY stream.
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

/// A recursive gate of matchers and sub-gates.
#[derive(Default)]
struct Gate {
    /// Case-insensitive substrings (stored lowercase).
    contains: Vec<&'static str>,
    regex: Vec<Regex>,
    all: Vec<Gate>,
    any: Vec<Gate>,
    not: Vec<Gate>,
}

impl Gate {
    /// Every direct matcher matches, every `all` sub-gate
    /// matches, at least one `any` sub-gate matches (or there are none),
    /// and no `not` sub-gate matches.
    fn matches(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|c| lower.contains(c))
            && self.regex.iter().all(|r| r.is_match(text))
            && self.all.iter().all(|g| g.matches(text, lower))
            && (self.any.is_empty() || self.any.iter().any(|g| g.matches(text, lower)))
            && !self.not.iter().any(|g| g.matches(text, lower))
    }
}

/// Target state plus priority.
struct Rule {
    state: AgentState,
    priority: u32,
    source: Source,
    gate: Gate,
}

fn contains(needles: &[&'static str]) -> Gate {
    Gate { contains: needles.to_vec(), ..Default::default() }
}

fn regex(patterns: &[&str]) -> Gate {
    Gate {
        regex: patterns.iter().map(|p| Regex::new(p).expect("valid rule regex")).collect(),
        ..Default::default()
    }
}

/// The hardcoded Claude Code rule set, evaluated against
/// the current snapshot on every new PTY output.
static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // Permission/confirmation prompts: Claude Code is waiting on the
        // user.
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
        // Any interactive prompt renders a numbered option list with a ❯
        // selector regardless of wording — AskUserQuestion, trust
        // dialogs, permission variants.
        Rule {
            state: AgentState::Blocked,
            priority: 890,
            source: Source::Screen,
            gate: regex(&["❯\\s*1\\."]),
        },
        // The CLI animates a Braille spinner into the window title while
        // it runs.
        Rule {
            state: AgentState::Working,
            priority: 850,
            source: Source::OscTitle,
            gate: regex(&["^[\u{2800}-\u{28FF}]"]),
        },
        // OSC 9;4 progress present in any form means active work.
        Rule {
            state: AgentState::Working,
            priority: 840,
            source: Source::OscProgress,
            gate: Gate {
                any: vec![contains(&["percentage"]), contains(&["indeterminate"])],
                ..Default::default()
            },
        },
        // The interrupt hint is only on screen while a turn is running,
        // and never while a permission prompt is up (blocked outranks it).
        Rule {
            state: AgentState::Working,
            priority: 800,
            source: Source::Screen,
            gate: contains(&["esc to interrupt"]),
        },
    ]
});

/// Evaluate every rule; the highest-priority match wins,
/// ties favoring the earliest declared. No match at all means idle.
pub fn evaluate(snapshot: &Snapshot) -> AgentState {
    let mut best: Option<&Rule> = None;
    for rule in RULES.iter() {
        let text = match rule.source {
            Source::Screen => &snapshot.screen,
            Source::OscTitle => &snapshot.title,
            Source::OscProgress => &snapshot.progress,
        };
        let lower = text.to_lowercase();
        if rule.gate.matches(text, &lower)
            && best.is_none_or(|b| rule.priority > b.priority)
        {
            best = Some(rule);
        }
    }
    best.map_or(AgentState::Idle, |r| r.state)
}

/// Per-tab agent display state: the debounced state the tab bar shows,
/// plus whether the user has seen the tab since it last became idle.
pub struct Tracker {
    displayed: AgentState,
    /// When a working/blocked tab first evaluated idle.
    pending_idle: Option<Instant>,
    seen: bool,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            displayed: AgentState::Idle,
            pending_idle: None,
            seen: true,
        }
    }
}

impl Tracker {
    /// Fold a fresh evaluation in; returns whether the displayed state
    /// changed. Transitions into idle are debounced and
    /// cancelled if the evidence moves off idle first.
    pub fn observe(&mut self, raw: AgentState, now: Instant) -> bool {
        if raw == self.displayed {
            self.pending_idle = None;
            return false;
        }
        if raw == AgentState::Idle {
            match self.pending_idle {
                Some(since) if now.duration_since(since) >= IDLE_DEBOUNCE => {
                    self.commit_idle();
                    true
                }
                Some(_) => false,
                None => {
                    self.pending_idle = Some(now);
                    false
                }
            }
        } else {
            // This also covers direct working↔blocked moves.
            self.pending_idle = None;
            self.displayed = raw;
            true
        }
    }

    /// Commit a pending idle whose debounce has elapsed with no further
    /// output arriving; returns whether the displayed state changed.
    pub fn tick(&mut self, now: Instant) -> bool {
        match self.pending_idle {
            Some(since) if now.duration_since(since) >= IDLE_DEBOUNCE => {
                self.commit_idle();
                true
            }
            _ => false,
        }
    }

    fn commit_idle(&mut self) {
        self.displayed = AgentState::Idle;
        self.pending_idle = None;
        // Freshly idle means not yet seen ("done").
        self.seen = false;
    }

    pub fn pending(&self) -> bool {
        self.pending_idle.is_some()
    }

    /// The tab is being displayed in the focused window.
    pub fn mark_seen(&mut self) {
        self.seen = true;
    }

    /// Bracketed status text and color per visual
    /// state — done is idle-but-unseen, idle is idle-and-seen. Working
    /// shimmers and blocked breathes; idle and done stay
    /// still.
    pub fn visual(&self) -> Visual {
        let (text, color, anim) = match (self.displayed, self.seen) {
            (AgentState::Working, _) => ("[working]", Color::Yellow, Anim::Shimmer),
            (AgentState::Blocked, _) => ("[blocked]", Color::Red, Anim::Breathe),
            (AgentState::Idle, false) => ("[done]", Color::Green, Anim::None),
            (AgentState::Idle, true) => ("[idle]", Color::DarkGray, Anim::None),
        };
        Visual { text, color, anim }
    }
}

/// A tab's bracketed status text as the tab bar draws it.
pub struct Visual {
    pub text: &'static str,
    pub color: Color,
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
    fn no_evidence_is_idle() {
        assert_eq!(evaluate(&snap("$ ls\nfoo bar\n", "bash", "none")), AgentState::Idle);
    }

    #[test]
    fn interrupt_hint_is_working() {
        let s = snap("✶ Herding… (esc to interrupt)\n", "", "none");
        assert_eq!(evaluate(&s), AgentState::Working);
    }

    #[test]
    fn spinner_title_is_working() {
        assert_eq!(evaluate(&snap("", "⠹ claude", "none")), AgentState::Working);
        assert_eq!(evaluate(&snap("", "claude", "none")), AgentState::Idle);
    }

    #[test]
    fn progress_is_working() {
        assert_eq!(evaluate(&snap("", "", "percentage:40")), AgentState::Working);
        assert_eq!(evaluate(&snap("", "", "indeterminate")), AgentState::Working);
        assert_eq!(evaluate(&snap("", "", "none")), AgentState::Idle);
    }

    #[test]
    fn permission_prompt_outranks_working_evidence() {
        // Both match; blocked has the higher priority.
        let s = snap(
            "Bash command…\nDo you want to proceed?\n❯ 1. Yes\n  2. No\n(esc to interrupt)\n",
            "⠹ claude",
            "none",
        );
        assert_eq!(evaluate(&s), AgentState::Blocked);
    }

    #[test]
    fn option_selector_prompt_is_blocked() {
        // Freeform prompts (AskUserQuestion, trust
        // dialogs) share only their selector chrome, not their wording.
        let s = snap(
            "Which library should we use?\n❯ 1. serde\n  2. nanoserde\n",
            "claude",
            "none",
        );
        assert_eq!(evaluate(&s), AgentState::Blocked);
        // The selector outranks leftover working evidence, like the
        // phrase rules do.
        let s = snap("Pick one\n❯ 1. Yes\n(esc to interrupt)\n", "", "none");
        assert_eq!(evaluate(&s), AgentState::Blocked);
        // A numbered list without the selector is ordinary output.
        let s = snap("1. serde\n2. nanoserde\n", "claude", "none");
        assert_eq!(evaluate(&s), AgentState::Idle);
    }

    #[test]
    fn gate_semantics_hold() {
        // Empty `any` passes; `not` excludes.
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
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert!(t.observe(AgentState::Working, t0));
        // First idle result arms the debounce without changing display.
        assert!(!t.observe(AgentState::Idle, t0));
        assert!(t.pending());
        // Evidence moves back to working → cancelled.
        assert!(!t.observe(AgentState::Working, t0 + Duration::from_millis(100)));
        assert!(!t.pending());
        assert_eq!(t.visual().text, "[working]");
        // Idle held past the debounce commits...
        assert!(!t.observe(AgentState::Idle, t0 + Duration::from_millis(200)));
        assert!(t.observe(AgentState::Idle, t0 + Duration::from_millis(200) + IDLE_DEBOUNCE));
        // ...and lands as done (unseen) until marked seen.
        assert_eq!(t.visual().text, "[done]");
        t.mark_seen();
        assert_eq!(t.visual().text, "[idle]");
    }

    #[test]
    fn tick_commits_a_quiet_pending_idle() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(AgentState::Working, t0);
        t.observe(AgentState::Idle, t0);
        assert!(!t.tick(t0 + Duration::from_millis(100)));
        assert!(t.tick(t0 + IDLE_DEBOUNCE));
        assert_eq!(t.visual().text, "[done]");
    }
}
