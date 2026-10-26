//! Task manager: which running apps may be stopped, and what a stop did.
//!
//! Pure. The command layer reads `dumpsys meminfo`, the package lists, the
//! Home handlers and two secure settings, and hands the facts in; this module
//! decides. It is the audited half of the Task Manager tab, the same way
//! [`super::safety`] is the audited half of disable.
//!
//! The verb is `am kill`, which Android itself limits to processes it already
//! considers expendable (service, previous and cached importance). On top of
//! that, nothing is offered unless all of these hold:
//!
//! - the process belongs to a package the device confirms is installed — a
//!   bare process name is never an app;
//! - the package is not on the protected or caution lists, not a Home app,
//!   not the active keyboard and not an enabled accessibility service;
//! - it is a user-installed app, or a preinstalled one the reviewed catalog
//!   rates safe to remove (stopping is milder than removing);
//! - at least one of its processes is in the background.
//!
//! Anything this module cannot read is treated as unknown, and unknown is
//! never offered.

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;

use super::launcher::LauncherCatalog;
use super::safety::{Safety, SafetySource};

/// How much Android is using a process right now, from the bucket it is
/// listed under in `Total PSS by OOM adjustment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    /// Native daemons, `system_server` and persistent framework processes.
    System,
    /// On screen and focused.
    Foreground,
    /// Visible, or bound by something that is.
    Visible,
    /// Noticeable without being on screen: playing audio, a foreground
    /// service, the active keyboard.
    Perceptible,
    /// A backup or a heavy-weight app Android is keeping alive.
    Busy,
    /// The current Home app.
    Home,
    /// Running a background service.
    Service,
    /// The app used just before the current one.
    Previous,
    /// Kept only so it reopens quickly. Android drops these first under
    /// memory pressure.
    Cached,
    /// A bucket this build names in a way we do not recognise.
    Unknown,
}

impl Importance {
    /// Map an OOM-adjustment heading. Headings differ a little across
    /// Android versions and vendors (`Visible Low`, `Perceptible Medium`), so
    /// the families are matched by prefix; anything else is `Unknown`.
    pub fn from_bucket(label: &str) -> Self {
        let label = label.trim();
        match label {
            "Native" | "System" | "Persistent" | "Persistent Service" => Self::System,
            "Foreground" => Self::Foreground,
            "Heavy Weight" | "Backup" => Self::Busy,
            "Home" => Self::Home,
            "A Services" | "B Services" => Self::Service,
            "Previous" => Self::Previous,
            "Cached" | "Empty" | "Hidden" => Self::Cached,
            _ if label.starts_with("Visible") => Self::Visible,
            _ if label.starts_with("Perceptible") => Self::Perceptible,
            _ => Self::Unknown,
        }
    }

    /// Lower is more in use. `Unknown` ranks with the in-use buckets, so an
    /// unrecognised heading never makes an app look idle.
    fn rank(self) -> u8 {
        match self {
            Self::System => 0,
            Self::Foreground => 1,
            Self::Visible => 2,
            Self::Perceptible => 3,
            Self::Busy => 4,
            Self::Unknown => 5,
            Self::Home => 6,
            Self::Service => 7,
            Self::Previous => 8,
            Self::Cached => 9,
        }
    }

    /// The buckets `am kill` acts on and this module offers. Home is left out
    /// on purpose even though Android would kill it.
    pub fn is_background(self) -> bool {
        matches!(self, Self::Service | Self::Previous | Self::Cached)
    }
}

/// One process from the OOM-adjustment section, as the command layer read it.
#[derive(Debug, Clone)]
pub struct RunningProcess {
    pub process: String,
    pub pid: Option<u32>,
    pub kb: u64,
    pub bucket: String,
    /// The package the process name would belong to, if it has the shape of
    /// one. Unverified until it is matched against the installed list.
    pub package: Option<String>,
}

/// What the device said about itself, beyond the process list.
pub struct DeviceFacts<'a> {
    pub installed: &'a HashSet<String>,
    /// `pm list packages -3`: installed by the user rather than the image.
    pub third_party: &'a HashSet<String>,
    /// Packages declaring a Home screen. `None` when the TV could not be
    /// asked (Android 8 has no `query-activities`); the known launchers and
    /// the Home bucket still apply then.
    pub home_handlers: Option<&'a HashSet<String>>,
    /// Package of `default_input_method`, if one is set.
    pub input_method: Option<&'a str>,
    /// Packages named in `enabled_accessibility_services`.
    pub accessibility: &'a HashSet<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskProcess {
    pub process: String,
    pub pid: Option<u32>,
    pub mb: f64,
    pub importance: Importance,
}

/// Why an app cannot be stopped from the Task Manager.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// On the do-not-disable list.
    Protected,
    /// On the caution list: remote, voice, accessibility plumbing.
    Sensitive,
    /// Declares a Home screen, or is a launcher we know.
    Launcher,
    /// The active keyboard.
    Keyboard,
    /// An enabled accessibility service.
    Accessibility,
    /// Part of the system image and not reviewed as safe to remove.
    SystemApp,
    /// Every process is in use right now.
    InUse,
    /// A process sits in a bucket this build names in a way we do not know.
    UnknownState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KillVerdict {
    Allowed,
    Blocked { reason: BlockReason, detail: String },
}

impl KillVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// One installed app with at least one running process.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskApp {
    pub package: String,
    /// Friendly name, filled in by the command layer from the reviewed lists.
    pub name: Option<String>,
    pub processes: Vec<TaskProcess>,
    pub total_mb: f64,
    /// Memory held by the app's background processes: roughly what a stop
    /// returns, since in-use processes are left running.
    pub background_mb: f64,
    /// The most in-use of its processes.
    pub importance: Importance,
    pub third_party: bool,
    pub verdict: KillVerdict,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskPlan {
    /// Largest first.
    pub apps: Vec<TaskApp>,
    /// Processes not tied to an installed package: daemons, HALs, isolated
    /// sandboxes. Shown for completeness; nothing here is ever offered.
    pub other: Vec<TaskProcess>,
}

fn mb(kb: u64) -> f64 {
    (kb as f64 / 1024.0 * 10.0).round() / 10.0
}

fn round_mb(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Group the running processes by installed package and decide, for each,
/// whether the Task Manager may stop it.
pub fn plan_tasks(
    processes: &[RunningProcess],
    facts: &DeviceFacts<'_>,
    launchers: &LauncherCatalog,
    safety: impl Fn(&str) -> Safety,
) -> TaskPlan {
    let mut grouped: BTreeMap<&str, Vec<TaskProcess>> = BTreeMap::new();
    let mut other = Vec::new();
    for p in processes {
        let row = TaskProcess {
            process: p.process.clone(),
            pid: p.pid,
            mb: mb(p.kb),
            importance: Importance::from_bucket(&p.bucket),
        };
        match p.package.as_deref() {
            Some(pkg) if facts.installed.contains(pkg) => grouped.entry(pkg).or_default().push(row),
            _ => other.push(row),
        }
    }

    let mut apps: Vec<TaskApp> = grouped
        .into_iter()
        .map(|(package, mut processes)| {
            processes.sort_by(|a, b| b.mb.total_cmp(&a.mb));
            let importance = processes
                .iter()
                .map(|p| p.importance)
                .min_by_key(|i| i.rank())
                .unwrap_or(Importance::Unknown);
            let total_mb = round_mb(processes.iter().map(|p| p.mb).sum());
            let background_mb = round_mb(
                processes
                    .iter()
                    .filter(|p| p.importance.is_background())
                    .map(|p| p.mb)
                    .sum(),
            );
            let third_party = facts.third_party.contains(package);
            let verdict = kill_verdict(
                package,
                &processes,
                third_party,
                facts,
                launchers,
                &safety(package),
            );
            TaskApp {
                package: package.to_string(),
                name: None,
                processes,
                total_mb,
                background_mb,
                importance,
                third_party,
                verdict,
            }
        })
        .collect();
    apps.sort_by(|a, b| {
        b.total_mb
            .total_cmp(&a.total_mb)
            .then_with(|| a.package.cmp(&b.package))
    });
    other.sort_by(|a, b| b.mb.total_cmp(&a.mb));
    TaskPlan { apps, other }
}

fn blocked(reason: BlockReason, detail: impl Into<String>) -> KillVerdict {
    KillVerdict::Blocked {
        reason,
        detail: detail.into(),
    }
}

/// The gate. Order matters: a permanent reason (protected, system) is more
/// useful to read than a passing one (in use right now).
fn kill_verdict(
    package: &str,
    processes: &[TaskProcess],
    third_party: bool,
    facts: &DeviceFacts<'_>,
    launchers: &LauncherCatalog,
    safety: &Safety,
) -> KillVerdict {
    match safety {
        Safety::NeverDisable { reason, .. } => {
            return blocked(
                BlockReason::Protected,
                format!("{reason} Stopping it is never offered."),
            )
        }
        Safety::Caution {
            reason,
            source: SafetySource::CautionList,
        } => {
            return blocked(
                BlockReason::Sensitive,
                format!("{reason} Stopping it is not offered."),
            )
        }
        _ => {}
    }

    let is_launcher = facts
        .home_handlers
        .is_some_and(|handlers| handlers.contains(package))
        || launchers.is_stock(package)
        || launchers.is_transient_home_holder(package)
        || launchers
            .custom
            .iter()
            .any(|entry| entry.package == package)
        || processes.iter().any(|p| p.importance == Importance::Home);
    if is_launcher {
        return blocked(
            BlockReason::Launcher,
            "A Home screen app. Android restarts the Home app as soon as it is stopped, so stopping it frees nothing and can blank the screen for a moment.",
        );
    }
    if facts.input_method == Some(package) {
        return blocked(
            BlockReason::Keyboard,
            "The keyboard this TV is set to use. Stopping it would interrupt typing and voice input.",
        );
    }
    if facts.accessibility.contains(package) {
        return blocked(
            BlockReason::Accessibility,
            "An enabled accessibility service. Remapped buttons and other assistive features stop working while it is stopped.",
        );
    }
    if !third_party && !matches!(safety, Safety::Safe { .. }) {
        return blocked(
            BlockReason::SystemApp,
            "Part of the TV's system image, and not reviewed as safe to remove. System apps are left alone.",
        );
    }
    if processes.iter().any(|p| p.importance.is_background()) {
        return KillVerdict::Allowed;
    }
    if processes
        .iter()
        .any(|p| p.importance == Importance::Unknown)
    {
        return blocked(
            BlockReason::UnknownState,
            "This TV reported its state in a way we do not recognise, so we cannot tell whether it is in use.",
        );
    }
    let top = processes
        .iter()
        .map(|p| p.importance)
        .min_by_key(|i| i.rank())
        .unwrap_or(Importance::Unknown);
    blocked(BlockReason::InUse, in_use_detail(top))
}

fn in_use_detail(importance: Importance) -> &'static str {
    match importance {
        Importance::Foreground => "On screen right now. Only background apps are stopped.",
        Importance::Visible => "Visible on screen, or working for something that is. Only background apps are stopped.",
        Importance::Perceptible => "Doing something you would notice, such as playing audio or running a foreground service. Only background apps are stopped.",
        Importance::Busy => "In the middle of a backup or other work Android is protecting. Only background apps are stopped.",
        _ => "Android is using it right now. Only background apps are stopped.",
    }
}

/// Force stop goes further than `am kill`: it ends every process, in-use ones
/// included, and cancels the app's alarms and jobs until it is opened again.
/// So on top of the kill gate, every process must already be in the
/// background.
pub fn force_stop_verdict(app: &TaskApp) -> KillVerdict {
    if !app.verdict.is_allowed() {
        return app.verdict.clone();
    }
    if app.processes.iter().all(|p| p.importance.is_background()) {
        return KillVerdict::Allowed;
    }
    blocked(
        BlockReason::InUse,
        "Part of it is in use right now, and force stop would end that too. Use Stop, which only ends the background part.",
    )
}

/// What one stop did, judged from a read before and a read after.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KillOutcome {
    /// Every targeted process is gone and nothing replaced it.
    Stopped { freed_mb: f64 },
    /// The targeted processes are gone, but the app started again on its own
    /// (a sticky service, a scheduled job).
    Restarted { freed_mb: f64, now_mb: f64 },
    /// Android kept some targeted processes alive: they became active, or
    /// the TV's Android declined the request.
    Kept { freed_mb: f64 },
    /// Never sent: the gate refused it.
    Refused { reason: BlockReason, detail: String },
    /// The app was not running when the request arrived.
    NotRunning,
    /// The device reported an error for the command.
    Failed { message: String },
    /// Sent, but the TV could not be read again to confirm what happened.
    Unverified,
}

/// Compare an app before and after a stop. `after` is `None` when it no
/// longer appears at all. Only processes the command targeted count: `am
/// kill` leaves in-use processes alone by design, so their survival is not a
/// failure.
pub fn kill_outcome(before: &TaskApp, after: Option<&TaskApp>, force: bool) -> KillOutcome {
    let targeted: Vec<&TaskProcess> = before
        .processes
        .iter()
        .filter(|p| p.pid.is_some() && (force || p.importance.is_background()))
        .collect();
    let before_pids: HashSet<u32> = before.processes.iter().filter_map(|p| p.pid).collect();
    let after_processes: &[TaskProcess] = after.map(|a| a.processes.as_slice()).unwrap_or(&[]);
    let after_pids: HashSet<u32> = after_processes.iter().filter_map(|p| p.pid).collect();

    let freed_mb = round_mb(
        targeted
            .iter()
            .filter(|p| p.pid.is_some_and(|pid| !after_pids.contains(&pid)))
            .map(|p| p.mb)
            .sum(),
    );
    if targeted
        .iter()
        .any(|p| p.pid.is_some_and(|pid| after_pids.contains(&pid)))
    {
        return KillOutcome::Kept { freed_mb };
    }
    let respawned: Vec<&TaskProcess> = after_processes
        .iter()
        .filter(|p| p.pid.is_some_and(|pid| !before_pids.contains(&pid)))
        .collect();
    if respawned.is_empty() {
        KillOutcome::Stopped { freed_mb }
    } else {
        KillOutcome::Restarted {
            freed_mb,
            now_mb: round_mb(respawned.iter().map(|p| p.mb).sum()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(process: &str, pid: u32, kb: u64, bucket: &str) -> RunningProcess {
        RunningProcess {
            process: process.into(),
            pid: Some(pid),
            kb,
            bucket: bucket.into(),
            package: process.split(':').next().map(str::to_string),
        }
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn unknown(_: &str) -> Safety {
        crate::engine::classify_safety("com.example.nothing.reviewed")
    }

    fn catalog() -> LauncherCatalog {
        LauncherCatalog::default()
    }

    struct World {
        installed: HashSet<String>,
        third_party: HashSet<String>,
        home: HashSet<String>,
        accessibility: HashSet<String>,
    }

    impl World {
        fn new() -> Self {
            Self {
                installed: set(&[
                    "com.example.player",
                    "com.example.music",
                    "com.example.launcher",
                    "com.example.keys",
                    "com.example.mapper",
                    "com.vendor.bloat",
                    "com.vendor.core",
                    "com.android.systemui",
                ]),
                third_party: set(&[
                    "com.example.player",
                    "com.example.music",
                    "com.example.launcher",
                    "com.example.keys",
                    "com.example.mapper",
                ]),
                home: set(&["com.example.launcher"]),
                accessibility: set(&["com.example.mapper"]),
            }
        }

        fn plan(&self, processes: &[RunningProcess], safety: impl Fn(&str) -> Safety) -> TaskPlan {
            let facts = DeviceFacts {
                installed: &self.installed,
                third_party: &self.third_party,
                home_handlers: Some(&self.home),
                input_method: Some("com.example.keys"),
                accessibility: &self.accessibility,
            };
            plan_tasks(processes, &facts, &catalog(), safety)
        }
    }

    fn verdict_of<'a>(plan: &'a TaskPlan, pkg: &str) -> &'a KillVerdict {
        &plan.apps.iter().find(|a| a.package == pkg).unwrap().verdict
    }

    fn reason(v: &KillVerdict) -> Option<BlockReason> {
        match v {
            KillVerdict::Allowed => None,
            KillVerdict::Blocked { reason, .. } => Some(*reason),
        }
    }

    #[test]
    fn buckets_map_by_family_and_unknown_never_looks_idle() {
        assert_eq!(Importance::from_bucket("Cached"), Importance::Cached);
        assert_eq!(
            Importance::from_bucket("  B Services "),
            Importance::Service
        );
        assert_eq!(Importance::from_bucket("Visible Low"), Importance::Visible);
        assert_eq!(
            Importance::from_bucket("Perceptible Medium"),
            Importance::Perceptible
        );
        assert_eq!(
            Importance::from_bucket("Persistent Service"),
            Importance::System
        );
        let odd = Importance::from_bucket("Frozen Somehow");
        assert_eq!(odd, Importance::Unknown);
        assert!(!odd.is_background());
        assert!(!Importance::Home.is_background());
    }

    #[test]
    fn user_app_in_the_background_is_offered_and_grouped() {
        let w = World::new();
        let plan = w.plan(
            &[
                proc("com.example.player", 10, 200 * 1024, "Cached"),
                proc("com.example.player:download", 11, 50 * 1024, "B Services"),
            ],
            unknown,
        );
        let app = &plan.apps[0];
        assert_eq!(app.package, "com.example.player");
        assert_eq!(app.processes.len(), 2);
        assert_eq!(app.total_mb, 250.0);
        assert_eq!(app.background_mb, 250.0);
        assert_eq!(app.importance, Importance::Service);
        assert!(app.verdict.is_allowed());
    }

    #[test]
    fn partially_in_use_app_offers_only_its_background_memory() {
        let w = World::new();
        let plan = w.plan(
            &[
                proc("com.example.music", 20, 100 * 1024, "Perceptible"),
                proc("com.example.music:cache", 21, 40 * 1024, "Cached"),
            ],
            unknown,
        );
        let app = &plan.apps[0];
        assert!(app.verdict.is_allowed());
        assert_eq!(app.background_mb, 40.0);
        assert_eq!(app.importance, Importance::Perceptible);
        assert_eq!(
            reason(&force_stop_verdict(app)),
            Some(BlockReason::InUse),
            "force stop would end the playing process too"
        );
    }

    #[test]
    fn in_use_app_is_not_offered() {
        let w = World::new();
        let plan = w.plan(
            &[proc("com.example.player", 10, 1024, "Foreground")],
            unknown,
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.example.player")),
            Some(BlockReason::InUse)
        );
    }

    #[test]
    fn unrecognised_bucket_is_unknown_not_idle() {
        let w = World::new();
        let plan = w.plan(&[proc("com.example.player", 10, 1024, "Mystery")], unknown);
        assert_eq!(
            reason(verdict_of(&plan, "com.example.player")),
            Some(BlockReason::UnknownState)
        );
    }

    #[test]
    fn launchers_keyboards_and_accessibility_are_never_offered() {
        let w = World::new();
        let plan = w.plan(
            &[
                proc("com.example.launcher", 1, 1024, "Cached"),
                proc("com.example.keys", 2, 1024, "Cached"),
                proc("com.example.mapper", 3, 1024, "Cached"),
            ],
            unknown,
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.example.launcher")),
            Some(BlockReason::Launcher)
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.example.keys")),
            Some(BlockReason::Keyboard)
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.example.mapper")),
            Some(BlockReason::Accessibility)
        );
    }

    #[test]
    fn home_bucket_marks_a_launcher_even_when_home_handlers_are_unreadable() {
        let w = World::new();
        let facts = DeviceFacts {
            installed: &w.installed,
            third_party: &w.third_party,
            home_handlers: None,
            input_method: None,
            accessibility: &w.accessibility,
        };
        let plan = plan_tasks(
            &[
                proc("com.example.player", 1, 1024, "Home"),
                proc("com.example.player:svc", 2, 1024, "Cached"),
            ],
            &facts,
            &catalog(),
            unknown,
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.example.player")),
            Some(BlockReason::Launcher)
        );
    }

    #[test]
    fn known_launchers_are_blocked_without_a_home_query() {
        let w = World::new();
        let mut launchers = catalog();
        launchers.stock.push(crate::engine::LauncherEntry {
            name: "Stock".into(),
            package: "com.vendor.bloat".into(),
            source_url: None,
            disable_with: vec![],
        });
        let facts = DeviceFacts {
            installed: &w.installed,
            third_party: &w.third_party,
            home_handlers: None,
            input_method: None,
            accessibility: &w.accessibility,
        };
        let plan = plan_tasks(
            &[proc("com.vendor.bloat", 1, 1024, "Cached")],
            &facts,
            &launchers,
            |_| Safety::Safe {
                reason: "reviewed".into(),
                source: SafetySource::ReviewedCatalog,
            },
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.vendor.bloat")),
            Some(BlockReason::Launcher)
        );
    }

    #[test]
    fn system_apps_need_a_safe_review_and_protected_always_wins() {
        let w = World::new();
        let processes = [
            proc("com.vendor.bloat", 1, 1024, "Cached"),
            proc("com.vendor.core", 2, 1024, "Cached"),
            proc("com.android.systemui", 3, 1024, "Cached"),
        ];
        let plan = w.plan(&processes, |pkg| match pkg {
            "com.vendor.bloat" => Safety::Safe {
                reason: "reviewed".into(),
                source: SafetySource::ReviewedCatalog,
            },
            "com.vendor.core" => Safety::Caution {
                reason: "medium".into(),
                source: SafetySource::ReviewedCatalog,
            },
            other => crate::engine::classify_safety(other),
        });
        assert!(verdict_of(&plan, "com.vendor.bloat").is_allowed());
        assert_eq!(
            reason(verdict_of(&plan, "com.vendor.core")),
            Some(BlockReason::SystemApp)
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.android.systemui")),
            Some(BlockReason::Protected)
        );
    }

    #[test]
    fn caution_list_blocks_even_a_user_app() {
        let mut w = World::new();
        w.installed.insert("com.google.android.katniss".into());
        w.third_party.insert("com.google.android.katniss".into());
        let plan = w.plan(
            &[proc("com.google.android.katniss", 1, 1024, "Cached")],
            crate::engine::classify_safety,
        );
        assert_eq!(
            reason(verdict_of(&plan, "com.google.android.katniss")),
            Some(BlockReason::Sensitive)
        );
    }

    #[test]
    fn unconfirmed_names_are_other_processes_never_apps() {
        let w = World::new();
        let plan = w.plan(
            &[
                proc("media.codec", 1, 1024, "Native"),
                proc("com.not.installed", 2, 4096, "Cached"),
                RunningProcess {
                    process: "surfaceflinger".into(),
                    pid: Some(3),
                    kb: 2048,
                    bucket: "Native".into(),
                    package: None,
                },
            ],
            unknown,
        );
        assert!(plan.apps.is_empty());
        assert_eq!(plan.other.len(), 3);
        assert_eq!(plan.other[0].process, "com.not.installed");
    }

    fn app(processes: &[(u32, f64, Importance)]) -> TaskApp {
        TaskApp {
            package: "com.example.player".into(),
            name: None,
            processes: processes
                .iter()
                .map(|(pid, mb, importance)| TaskProcess {
                    process: "com.example.player".into(),
                    pid: Some(*pid),
                    mb: *mb,
                    importance: *importance,
                })
                .collect(),
            total_mb: 0.0,
            background_mb: 0.0,
            importance: Importance::Cached,
            third_party: true,
            verdict: KillVerdict::Allowed,
        }
    }

    #[test]
    fn outcomes_distinguish_stopped_restarted_and_kept() {
        let before = app(&[
            (10, 100.0, Importance::Cached),
            (11, 20.0, Importance::Service),
        ]);
        assert_eq!(
            kill_outcome(&before, None, false),
            KillOutcome::Stopped { freed_mb: 120.0 }
        );
        let respawn = app(&[(42, 30.0, Importance::Service)]);
        assert_eq!(
            kill_outcome(&before, Some(&respawn), false),
            KillOutcome::Restarted {
                freed_mb: 120.0,
                now_mb: 30.0
            }
        );
        let survived = app(&[(10, 100.0, Importance::Visible)]);
        assert_eq!(
            kill_outcome(&before, Some(&survived), false),
            KillOutcome::Kept { freed_mb: 20.0 }
        );
    }

    #[test]
    fn in_use_process_surviving_kill_is_not_a_failure() {
        let before = app(&[
            (10, 100.0, Importance::Perceptible),
            (11, 20.0, Importance::Cached),
        ]);
        let after = app(&[(10, 100.0, Importance::Perceptible)]);
        assert_eq!(
            kill_outcome(&before, Some(&after), false),
            KillOutcome::Stopped { freed_mb: 20.0 }
        );
        assert_eq!(
            kill_outcome(&before, Some(&after), true),
            KillOutcome::Kept { freed_mb: 20.0 },
            "force stop targets every process, so a survivor counts"
        );
    }
}
