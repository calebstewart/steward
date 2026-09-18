//! Which units start when, and in what order, and what stops with what.
//!
//! Four targets are built in, and the manager finds out for itself when each
//! is reached. `default.target` is reached as soon as the manager is up, at
//! sign-in; `timers.target`, where timers are installed, is another name for
//! it. `graphical-session.target` is reached once the shell is ready --
//! Explorer's taskbar exists -- before which there are no windows to manage.
//! `tray.target` is reached once the tray takes icons, a moment later. A unit
//! is started when a target it is `WantedBy=` is reached, along with
//! everything it `Wants=` or `Requires=`.
//!
//! Any other target is a unit file of its own, a unit that runs nothing, and
//! in the plan it is a unit like any other: `WantedBy=` it is its `Wants=`,
//! so starting it starts what it wants, and ordering after it is ordering
//! after a unit.
//!
//! A timer is a unit like any other too. What it starts when it elapses is
//! ordered after it, as in systemd, and nothing else binds them: stopping
//! the timer leaves a run it started running.
//!
//! Ordering is `After=`/`Before=`: a unit waits while anything it is ordered
//! after is still waiting or on its way up (a unit waiting out a restart
//! delay counts as on its way up). `Requires=` adds that the unit fails,
//! rather than starts, when what it requires has failed -- or waits, for a
//! built-in target not reached yet. Stopping everything (sign-out) runs the
//! order backwards.
//!
//! Stopping or restarting a unit on purpose does the same to what
//! `Requires=` it or is `PartOf=` it, as systemd propagates them; what it
//! merely `Wants=` is left alone.
//!
//! A unit that is up and in its steady state, a oneshot that finished, or one
//! that failed has settled; an ordering cycle is broken by starting (or
//! stopping) one of its units anyway.

use std::collections::{BTreeMap, BTreeSet};

use steward_unit::{Service, BUILTIN_TARGETS};
pub use steward_unit::{DEFAULT_TARGET, GRAPHICAL_TARGET, TIMERS_TARGET, TRAY_TARGET};

use crate::machine::State;

/// Reached by the manager, not started: `default.target` and
/// `timers.target` (which is the former), `graphical-session.target`, and
/// `tray.target`.
fn is_builtin(name: &str) -> bool {
    BUILTIN_TARGETS.contains(&name)
}

/// The name a unit's dependency means: `timers.target` is sign-in's.
fn canonical(name: &str) -> String {
    if name == TIMERS_TARGET {
        DEFAULT_TARGET.to_owned()
    } else {
        name.to_owned()
    }
}

/// Where a unit is, as far as ordering cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Idle,
    Starting,
    Active,
    Stopping,
    Failed,
}

impl From<State> for Progress {
    fn from(state: State) -> Self {
        match state {
            State::Inactive => Progress::Idle,
            State::Failed => Progress::Failed,
            // A reload leaves it up: nothing waits for it to finish.
            State::Active | State::Reloading(_) => Progress::Active,
            // It is coming back: what is ordered after it waits for it.
            State::AutoRestart => Progress::Starting,
            s if s.is_starting() => Progress::Starting,
            _ => Progress::Stopping,
        }
    }
}

impl Progress {
    fn is_running(self) -> bool {
        matches!(
            self,
            Progress::Starting | Progress::Active | Progress::Stopping
        )
    }
}

/// What to do with a unit that is waiting to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Start,
    /// Started despite an ordering it cannot satisfy: the units in a cycle.
    StartBreakingCycle,
    /// Cannot start: it requires a unit that failed or does not exist.
    Fail {
        missing: String,
    },
}

#[derive(Debug, Clone, Default)]
struct Node {
    after: BTreeSet<String>,
    wants: BTreeSet<String>,
    requires: BTreeSet<String>,
    part_of: BTreeSet<String>,
    /// Built-in targets only: a custom target's `WantedBy=` is its `Wants=`.
    wanted_by: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    nodes: BTreeMap<String, Node>,
}

fn names(list: &[String]) -> BTreeSet<String> {
    list.iter().map(|n| canonical(n)).collect()
}

impl Plan {
    /// The plan for these units -- services and targets -- and a warning for
    /// each dependency that leads nowhere.
    pub fn new<'a>(services: impl IntoIterator<Item = &'a Service>) -> (Plan, Vec<String>) {
        let services: Vec<&Service> = services.into_iter().collect();
        let mut nodes: BTreeMap<String, Node> = services
            .iter()
            .map(|s| {
                let node = Node {
                    after: names(&s.after),
                    wants: names(&s.wants),
                    requires: names(&s.requires),
                    part_of: names(&s.part_of),
                    wanted_by: names(&s.wanted_by)
                        .into_iter()
                        .filter(|t| is_builtin(t))
                        .collect(),
                };
                (s.name.clone(), node)
            })
            .collect();
        for s in &services {
            // `Before=` is `After=` seen from the other side.
            for later in names(&s.before) {
                if let Some(node) = nodes.get_mut(&later) {
                    node.after.insert(s.name.clone());
                }
            }
            // A unit `WantedBy=` a unit of the plan -- a target of the user's
            // -- is one it wants.
            for wanter in names(&s.wanted_by) {
                if let Some(node) = nodes.get_mut(&wanter) {
                    node.wants.insert(s.name.clone());
                }
            }
            // What a timer starts is ordered after it.
            if let Some(node) = s.timer.as_ref().and_then(|t| nodes.get_mut(&t.unit)) {
                node.after.insert(s.name.clone());
            }
        }

        let mut warnings = Vec::new();
        let exists = |n: &str| nodes.contains_key(n) || is_builtin(n);
        for s in &services {
            let name = &s.name;
            for target in names(&s.after).iter().chain(&names(&s.wanted_by)) {
                if target.ends_with(".target") && !exists(target) {
                    warnings.push(format!(
                        "{name}: {target} is neither steward's ({}) nor a unit file",
                        BUILTIN_TARGETS.join(", ")
                    ));
                }
            }
            for wanted in names(&s.wants) {
                if !exists(&wanted) {
                    warnings.push(format!("{name}: wants {wanted}, which does not exist"));
                }
            }
            for required in names(&s.requires) {
                if !exists(&required) {
                    warnings.push(format!(
                        "{name}: requires {required}, which does not exist; it will not start"
                    ));
                }
            }
            for whole in names(&s.part_of) {
                if !exists(&whole) {
                    warnings.push(format!("{name}: part of {whole}, which does not exist"));
                }
            }
            if let Some(timer) = s.timer.as_ref().filter(|t| !exists(&t.unit)) {
                warnings.push(format!(
                    "{name}: starts {}, which does not exist",
                    timer.unit
                ));
            }
        }
        (Plan { nodes }, warnings)
    }

    pub fn contains(&self, unit: &str) -> bool {
        self.nodes.contains_key(unit)
    }

    /// The units reaching `target` starts: those `WantedBy=` it, and what they
    /// want or require, and so on.
    pub fn pulled_in_by(&self, target: &str) -> BTreeSet<String> {
        let roots = self
            .nodes
            .iter()
            .filter(|(_, node)| node.wanted_by.contains(target))
            .map(|(name, _)| name.clone());
        self.closure(roots)
    }

    /// `unit` and what it wants or requires, and so on.
    pub fn with_dependencies(&self, unit: &str) -> BTreeSet<String> {
        self.closure([unit.to_owned()])
    }

    /// What stopping or restarting `unit` on purpose also stops or restarts:
    /// the units that require it or are part of it, and so on. Not `unit`.
    pub fn bound_to(&self, unit: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut todo = vec![canonical(unit)];
        while let Some(name) = todo.pop() {
            for (other, node) in &self.nodes {
                if (node.requires.contains(&name) || node.part_of.contains(&name))
                    && other != unit
                    && found.insert(other.clone())
                {
                    todo.push(other.clone());
                }
            }
        }
        found
    }

    fn closure(&self, roots: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut todo: Vec<String> = roots.into_iter().collect();
        while let Some(name) = todo.pop() {
            let Some(node) = self.nodes.get(&name) else {
                continue;
            };
            if found.insert(name) {
                todo.extend(node.wants.iter().chain(&node.requires).cloned());
            }
        }
        found
    }

    /// Of the units waiting to start, those that may start now, and those that
    /// never can.
    pub fn ready_to_start(
        &self,
        waiting: &BTreeSet<String>,
        progress: impl Fn(&str) -> Progress,
        reached: impl Fn(&str) -> bool,
    ) -> Vec<(String, Decision)> {
        let mut out = Vec::new();
        let mut held_by_target = BTreeSet::new();
        for name in waiting {
            let Some(node) = self.nodes.get(name) else {
                continue;
            };
            let broken = node.requires.iter().find(|r| {
                !is_builtin(r)
                    && (!self.nodes.contains_key(*r)
                        || (progress(r) == Progress::Failed && !waiting.contains(*r)))
            });
            if let Some(missing) = broken {
                out.push((
                    name.clone(),
                    Decision::Fail {
                        missing: missing.clone(),
                    },
                ));
                continue;
            }
            let mut blocked = false;
            // A built-in target is waited for, whether it is ordered after or
            // required; a unit (a target of the user's included) only while
            // it is on its way up.
            for before in node.after.iter().chain(&node.requires) {
                if is_builtin(before) {
                    if !reached(before) {
                        blocked = true;
                        held_by_target.insert(name.clone());
                    }
                } else if node.after.contains(before)
                    && (waiting.contains(before) || progress(before) == Progress::Starting)
                {
                    blocked = true;
                }
            }
            if !blocked {
                out.push((name.clone(), Decision::Start));
            }
        }

        let anything_starting = self
            .nodes
            .keys()
            .any(|name| !waiting.contains(name) && progress(name) == Progress::Starting);
        if out.is_empty() && !anything_starting {
            // Nothing can move and nothing will: unless they are waiting for a
            // target, the waiting units are in a cycle.
            if let Some(name) = waiting
                .iter()
                .find(|n| !held_by_target.contains(*n) && self.nodes.contains_key(*n))
            {
                out.push((name.clone(), Decision::StartBreakingCycle));
            }
        }
        out
    }

    /// Of the units waiting to stop, those that may stop now: nothing still
    /// running that is ordered after them is also waiting to stop.
    pub fn ready_to_stop(
        &self,
        waiting: &BTreeSet<String>,
        progress: impl Fn(&str) -> Progress,
    ) -> Vec<String> {
        let blocks = |later: &String| {
            (waiting.contains(later) && progress(later).is_running())
                || progress(later) == Progress::Stopping
        };
        let mut out: Vec<String> = waiting
            .iter()
            .filter(|name| {
                !self.nodes.iter().any(|(later, node)| {
                    later != *name && node.after.contains(*name) && blocks(later)
                })
            })
            .cloned()
            .collect();
        let anything_stopping = self.nodes.keys().any(|n| progress(n) == Progress::Stopping);
        if out.is_empty() && !anything_stopping {
            out.extend(waiting.iter().next().cloned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use steward_unit::parse_service;

    fn unit(name: &str, extra_unit: &str, install: &str) -> Service {
        let text = format!("[Unit]\n{extra_unit}\n[Service]\nExecStart=x\n[Install]\n{install}\n");
        let parsed = parse_service(name, &text);
        assert!(!parsed.has_errors(), "{:?}", parsed.diagnostics);
        parsed.service.unwrap()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Runs start decisions to completion, with every started unit becoming
    /// active (or failing, if named in `failing`) before the next round.
    struct Sim {
        plan: Plan,
        progress: RefCell<BTreeMap<String, Progress>>,
        reached: BTreeSet<String>,
        rounds: Vec<Vec<String>>,
    }

    impl Sim {
        fn new(services: &[Service]) -> Sim {
            let (plan, _) = Plan::new(services);
            Sim {
                plan,
                progress: RefCell::default(),
                reached: set(&[DEFAULT_TARGET]),
                rounds: Vec::new(),
            }
        }

        fn get(&self, name: &str) -> Progress {
            *self.progress.borrow().get(name).unwrap_or(&Progress::Idle)
        }

        fn start(&mut self, waiting: &mut BTreeSet<String>, failing: &[&str]) {
            loop {
                let decisions = self.plan.ready_to_start(
                    waiting,
                    |n| self.get(n),
                    |t| self.reached.contains(t),
                );
                if decisions.is_empty() {
                    break;
                }
                let mut round = Vec::new();
                for (name, decision) in decisions {
                    waiting.remove(&name);
                    let progress = match decision {
                        Decision::Fail { .. } => Progress::Failed,
                        _ if failing.contains(&name.as_str()) => Progress::Failed,
                        _ => Progress::Active,
                    };
                    round.push(match decision {
                        Decision::Start => name.clone(),
                        Decision::StartBreakingCycle => format!("{name} (cycle)"),
                        Decision::Fail { missing } => format!("{name} (needs {missing})"),
                    });
                    self.progress.borrow_mut().insert(name, progress);
                }
                self.rounds.push(round);
            }
        }
    }

    #[test]
    fn targets_pull_in_what_is_wanted_and_required() {
        let services = [
            unit("a.service", "Wants=b.service", "WantedBy=default.target"),
            unit("b.service", "Requires=c.service", ""),
            unit("c.service", "", ""),
            unit("d.service", "", "WantedBy=graphical-session.target"),
            unit("e.service", "", ""),
        ];
        let (plan, warnings) = Plan::new(&services);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            plan.pulled_in_by(DEFAULT_TARGET),
            set(&["a.service", "b.service", "c.service"])
        );
        assert_eq!(plan.pulled_in_by(GRAPHICAL_TARGET), set(&["d.service"]));
        assert_eq!(
            plan.with_dependencies("b.service"),
            set(&["b.service", "c.service"])
        );
    }

    #[test]
    fn after_orders_the_start_and_before_is_its_mirror() {
        let services = [
            unit("komorebi.service", "", ""),
            unit("bar.service", "After=komorebi.service", ""),
            unit("whkd.service", "Before=bar.service", ""),
            unit("free.service", "", ""),
        ];
        let mut sim = Sim::new(&services);
        let mut waiting = set(&[
            "bar.service",
            "komorebi.service",
            "whkd.service",
            "free.service",
        ]);
        sim.start(&mut waiting, &[]);
        assert_eq!(
            sim.rounds,
            [
                vec!["free.service", "komorebi.service", "whkd.service"],
                vec!["bar.service"]
            ]
        );
    }

    #[test]
    fn a_unit_waits_for_the_shell_if_it_says_so() {
        let services = [unit("tray.service", "After=graphical-session.target", "")];
        let mut sim = Sim::new(&services);
        let mut waiting = set(&["tray.service"]);
        sim.start(&mut waiting, &[]);
        assert!(
            sim.rounds.is_empty(),
            "no cycle-breaking while a target is pending"
        );
        sim.reached.insert(GRAPHICAL_TARGET.into());
        sim.start(&mut waiting, &[]);
        assert_eq!(sim.rounds, [vec!["tray.service"]]);
    }

    #[test]
    fn a_unit_waits_while_what_it_follows_is_starting_or_restarting() {
        let services = [
            unit("a.service", "", ""),
            unit("b.service", "After=a.service", ""),
        ];
        let (plan, _) = Plan::new(&services);
        let waiting = set(&["b.service"]);
        for (progress, ready) in [
            (Progress::Starting, false),
            (Progress::Active, true),
            (Progress::Failed, true),
            (Progress::Idle, true),
        ] {
            let got = plan.ready_to_start(
                &waiting,
                |n| {
                    if n == "a.service" {
                        progress
                    } else {
                        Progress::Idle
                    }
                },
                |_| true,
            );
            assert_eq!(!got.is_empty(), ready, "{progress:?}");
        }
        assert_eq!(Progress::from(State::AutoRestart), Progress::Starting);
    }

    #[test]
    fn requires_fails_the_dependent_when_the_dependency_fails() {
        let services = [
            unit("db.service", "", ""),
            unit("app.service", "Requires=db.service\nAfter=db.service", ""),
            unit("loose.service", "Wants=db.service\nAfter=db.service", ""),
        ];
        let mut sim = Sim::new(&services);
        let mut waiting = set(&["db.service", "app.service", "loose.service"]);
        sim.start(&mut waiting, &["db.service"]);
        assert_eq!(
            sim.rounds,
            [
                vec!["db.service"],
                vec!["app.service (needs db.service)", "loose.service"]
            ]
        );
    }

    #[test]
    fn requiring_a_unit_that_does_not_exist() {
        let services = [unit(
            "app.service",
            "Requires=ghost.service\nWants=spirit.service",
            "",
        )];
        let (plan, warnings) = Plan::new(&services);
        assert_eq!(
            warnings,
            [
                "app.service: wants spirit.service, which does not exist",
                "app.service: requires ghost.service, which does not exist; it will not start",
            ]
        );
        let got = plan.ready_to_start(&set(&["app.service"]), |_| Progress::Idle, |_| true);
        assert_eq!(
            got,
            [(
                "app.service".into(),
                Decision::Fail {
                    missing: "ghost.service".into()
                }
            )]
        );
    }

    #[test]
    fn unknown_targets_are_warned_about() {
        let services = [unit(
            "a.service",
            "After=network-online.target",
            "WantedBy=multi-user.target",
        )];
        let (_, warnings) = Plan::new(&services);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("network-online.target"));
    }

    #[test]
    fn an_ordering_cycle_is_broken() {
        let services = [
            unit("a.service", "After=b.service", ""),
            unit("b.service", "After=a.service", ""),
            unit("c.service", "After=b.service", ""),
        ];
        let mut sim = Sim::new(&services);
        let mut waiting = set(&["a.service", "b.service", "c.service"]);
        sim.start(&mut waiting, &[]);
        assert_eq!(
            sim.rounds,
            [
                vec!["a.service (cycle)"],
                vec!["b.service"],
                vec!["c.service"]
            ]
        );
    }

    #[test]
    fn the_examples_depend_on_nothing_missing() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let services: Vec<Service> = steward_unit::load_dir(&dir)
            .unwrap()
            .into_iter()
            .filter_map(|u| u.parsed.service)
            .collect();
        let (plan, warnings) = Plan::new(&services);
        assert!(warnings.is_empty(), "{warnings:?}");
        let graphical = plan.pulled_in_by(GRAPHICAL_TARGET);
        assert!(graphical.contains("after-ping.service"));
        // whkd comes with its group.
        assert!(graphical.contains("whkd.service"));
        assert_eq!(plan.bound_to("tiling.target"), set(&["whkd.service"]));
        // The timer comes with sign-in; what it starts, only when it elapses.
        let signed_in = plan.pulled_in_by(DEFAULT_TARGET);
        assert!(signed_in.contains("hello.timer"));
        assert!(!signed_in.contains("hello.service"));
    }

    fn target(name: &str, text: &str) -> Service {
        let parsed = parse_service(name, text);
        assert!(!parsed.has_errors(), "{:?}", parsed.diagnostics);
        parsed.service.unwrap()
    }

    #[test]
    fn a_target_of_the_user_s_wants_what_is_wanted_by_it() {
        let units = [
            target(
                "tiling.target",
                "[Unit]\nWants=bar.service\n[Install]\nWantedBy=graphical-session.target\n",
            ),
            unit(
                "komorebi.service",
                "PartOf=tiling.target",
                "WantedBy=tiling.target",
            ),
            unit("whkd.service", "", "WantedBy=tiling.target"),
            unit("bar.service", "After=komorebi.service", ""),
            unit("other.service", "", ""),
        ];
        let (plan, warnings) = Plan::new(&units);
        assert!(warnings.is_empty(), "{warnings:?}");
        let group = set(&[
            "tiling.target",
            "komorebi.service",
            "whkd.service",
            "bar.service",
        ]);
        assert_eq!(plan.with_dependencies("tiling.target"), group);
        assert_eq!(plan.pulled_in_by(GRAPHICAL_TARGET), group);
    }

    #[test]
    fn tray_target_is_its_own_and_comes_after_the_shell() {
        let services = [
            unit(
                "applet.service",
                "After=tray.target\nRequires=tray.target",
                "WantedBy=tray.target",
            ),
            // As home-manager writes a tray program: pulled in with the
            // session, ordered after the tray.
            unit(
                "hm-applet.service",
                "After=graphical-session.target tray.target",
                "WantedBy=graphical-session.target",
            ),
        ];
        let (plan, warnings) = Plan::new(&services);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(plan.pulled_in_by(TRAY_TARGET), set(&["applet.service"]));
        assert_eq!(
            plan.pulled_in_by(GRAPHICAL_TARGET),
            set(&["hm-applet.service"])
        );
        let mut sim = Sim::new(&services);
        let mut waiting = set(&["applet.service", "hm-applet.service"]);
        sim.reached.insert(GRAPHICAL_TARGET.into());
        sim.start(&mut waiting, &[]);
        assert!(
            sim.rounds.is_empty(),
            "both wait for the tray, and neither fails"
        );
        sim.reached.insert(TRAY_TARGET.into());
        sim.start(&mut waiting, &[]);
        assert_eq!(sim.rounds, [vec!["applet.service", "hm-applet.service"]]);
    }

    #[test]
    fn stopping_on_purpose_takes_what_requires_or_is_part_of_it() {
        let units = [
            target("tiling.target", "[Unit]\n"),
            unit(
                "komorebi.service",
                "PartOf=tiling.target",
                "WantedBy=tiling.target",
            ),
            unit("bar.service", "Requires=komorebi.service", ""),
            unit("whkd.service", "", "WantedBy=tiling.target"),
        ];
        let (plan, _) = Plan::new(&units);
        // whkd is only wanted by the target: it stays.
        assert_eq!(
            plan.bound_to("tiling.target"),
            set(&["komorebi.service", "bar.service"])
        );
        assert_eq!(plan.bound_to("komorebi.service"), set(&["bar.service"]));
        assert!(plan.bound_to("whkd.service").is_empty());
    }

    #[test]
    fn a_target_nobody_wrote_is_warned_about() {
        let services = [unit(
            "a.service",
            "PartOf=tiling.target",
            "WantedBy=tiling.target",
        )];
        let (_, warnings) = Plan::new(&services);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn timers_come_with_sign_in_and_what_they_start_follows_them() {
        let units = [
            target(
                "backup.timer",
                "[Timer]\nOnCalendar=daily\n[Install]\nWantedBy=timers.target\n",
            ),
            unit("backup.service", "", ""),
        ];
        let (plan, warnings) = Plan::new(&units);
        assert!(warnings.is_empty(), "{warnings:?}");
        // timers.target is reached at sign-in; the service is started by
        // the timer, not with it.
        assert_eq!(plan.pulled_in_by(DEFAULT_TARGET), set(&["backup.timer"]));
        let mut sim = Sim::new(&units);
        let mut waiting = set(&["backup.timer", "backup.service"]);
        sim.start(&mut waiting, &[]);
        assert_eq!(sim.rounds, [vec!["backup.timer"], vec!["backup.service"]]);
        // Nothing binds the two: stopping either leaves the other.
        assert!(plan.bound_to("backup.timer").is_empty());
        assert!(plan.bound_to("backup.service").is_empty());
    }

    #[test]
    fn a_timer_that_starts_nothing_is_warned_about() {
        let units = [target("backup.timer", "[Timer]\nOnCalendar=daily\n")];
        let (_, warnings) = Plan::new(&units);
        assert_eq!(
            warnings,
            ["backup.timer: starts backup.service, which does not exist"]
        );
    }

    #[test]
    fn stopping_runs_the_order_backwards() {
        let services = [
            unit("komorebi.service", "", ""),
            unit("bar.service", "After=komorebi.service", ""),
            unit("whkd.service", "", ""),
        ];
        let (plan, _) = Plan::new(&services);
        let progress = RefCell::new(BTreeMap::from([
            ("komorebi.service".to_string(), Progress::Active),
            ("bar.service".to_string(), Progress::Active),
            ("whkd.service".to_string(), Progress::Active),
        ]));
        let get = |n: &str| progress.borrow()[n];
        let mut waiting = set(&["komorebi.service", "bar.service", "whkd.service"]);

        let first = plan.ready_to_stop(&waiting, get);
        assert_eq!(first, ["bar.service", "whkd.service"]);
        for name in &first {
            waiting.remove(name);
            progress
                .borrow_mut()
                .insert(name.clone(), Progress::Stopping);
        }
        // komorebi waits while the bar is still stopping.
        assert!(plan.ready_to_stop(&waiting, get).is_empty());
        progress
            .borrow_mut()
            .insert("bar.service".into(), Progress::Idle);
        assert_eq!(plan.ready_to_stop(&waiting, get), ["komorebi.service"]);
    }
}
