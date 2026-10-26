//! Task Manager: list the TV's running apps with their memory and state, and
//! stop background ones with `am kill`.
//!
//! Every decision is made by [`crate::engine::tasks`]. The stop command never
//! trusts the list the UI is showing: it reads the device again, gates each
//! package against that fresh read, and only then sends anything. It reads
//! once more afterwards so the result says what actually happened.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::adb::{
    checked_batch_command, package_for_process, parse_checked_batch,
    parse_installed_packages_output, parse_oom_adjustment, MemoryMetric, RamInfo, BATCH_SEPARATOR,
    BATCH_STATUS,
};
use crate::engine::{
    force_stop_verdict, is_valid_package_name, kill_outcome, plan_tasks, DeviceFacts, KillOutcome,
    KillVerdict, RunningProcess, TaskApp, TaskProcess,
};

use super::launcher::{parse_home_handler_packages, HOME_HANDLER_QUERY};
use super::AppState;

/// Most packages one stop request may name. Far above any real TV's running
/// app count; it only bounds a malformed request.
const MAX_STOP_BATCH: usize = 64;

/// How long to let Android reap the stopped processes before reading again.
const SETTLE: Duration = Duration::from_millis(1200);

const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize)]
pub struct TaskSnapshot {
    /// PSS where the TV reports it, else RSS.
    pub metric: MemoryMetric,
    pub ram: RamInfo,
    pub apps: Vec<TaskApp>,
    pub other: Vec<TaskProcess>,
    /// False when the TV could not list its Home apps (Android 8). Known
    /// launchers and the current Home app are still recognised then.
    pub home_handlers_read: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    /// `am kill`: ends background processes only.
    Kill,
    /// `am force-stop`: ends every process and cancels alarms and jobs until
    /// the app is opened again. Offered only when nothing of it is in use.
    ForceStop,
}

#[derive(Debug, Serialize)]
pub struct StopResult {
    pub package: String,
    pub outcome: KillOutcome,
}

#[derive(Debug, Serialize)]
pub struct StopReport {
    pub results: Vec<StopResult>,
    pub ram_before: RamInfo,
    /// The read taken after the stop. `None` when it failed; the results
    /// then say "unverified" rather than guessing.
    pub snapshot: Option<TaskSnapshot>,
    pub snapshot_error: Option<String>,
}

/// `task_snapshot` — running apps grouped by package, with memory, state and
/// whether each may be stopped. Read-only.
#[tauri::command]
pub async fn task_snapshot(
    state: State<'_, AppState>,
    serial: String,
) -> Result<TaskSnapshot, String> {
    read_snapshot(state.inner(), &serial).await
}

/// `stop_background_apps` — stop each named package that passes the gate on
/// a fresh read. Packages that fail it are reported as refused, never sent.
#[tauri::command]
pub async fn stop_background_apps(
    state: State<'_, AppState>,
    serial: String,
    packages: Vec<String>,
    mode: StopMode,
) -> Result<StopReport, String> {
    stop_apps_impl(state.inner(), &serial, packages, mode, SETTLE).await
}

pub async fn read_snapshot(state: &AppState, serial: &str) -> Result<TaskSnapshot, String> {
    let adb = state.adb_snapshot().await;
    let cmd = checked_batch_command(&[
        "dumpsys meminfo",
        "pm list packages",
        "pm list packages -3",
        "settings get secure default_input_method",
        "settings get secure enabled_accessibility_services",
        HOME_HANDLER_QUERY,
        "cat /proc/meminfo",
    ]);
    let out = tokio::time::timeout(READ_TIMEOUT, adb.shell(serial, &cmd))
        .await
        .map_err(|_| "Reading running apps timed out after 30 seconds.".to_string())?
        .map_err(|e| format!("running apps: {e}"))?;
    // meminfo is not "required": that would also reject the whole read for one
    // process's error line in the dump. A failed meminfo comes back empty and
    // is caught below instead.
    let sections = parse_checked_batch(&out.stdout, 7, &[1, 2, 3, 4])?;
    if sections[0].trim().is_empty() {
        return Err("dumpsys meminfo returned nothing. Retry the read.".into());
    }
    let (metric, rows) = parse_oom_adjustment(&sections[0]).ok_or_else(|| {
        "This TV's memory report has no per-app breakdown, so running apps can't be listed."
            .to_string()
    })?;
    let installed: HashSet<String> = parse_installed_packages_output(&sections[1])
        .into_iter()
        .collect();
    // A TV always has installed packages. An empty list would turn every app
    // into an anonymous process, so it is a failed read, not a fact.
    if installed.is_empty() {
        return Err("pm list packages: no packages reported".into());
    }
    let third_party: HashSet<String> = parse_installed_packages_output(&sections[2])
        .into_iter()
        .collect();
    let input_method = component_packages(&sections[3]).into_iter().next();
    let accessibility: HashSet<String> = component_packages(&sections[4]).into_iter().collect();
    let home: HashSet<String> = parse_home_handler_packages(&sections[5])
        .into_iter()
        .collect();
    // Every Android build has at least Settings' fallback Home, so an empty
    // answer means the query is unsupported, not that nothing handles Home.
    let home_handlers = (!home.is_empty()).then_some(&home);

    let processes: Vec<RunningProcess> = rows
        .into_iter()
        .map(|row| RunningProcess {
            package: package_for_process(&row.process).map(str::to_string),
            process: row.process,
            pid: row.pid,
            kb: row.kb,
            bucket: row.bucket,
        })
        .collect();
    let facts = DeviceFacts {
        installed: &installed,
        third_party: &third_party,
        home_handlers,
        input_method: input_method.as_deref(),
        accessibility: &accessibility,
    };
    let mut plan = plan_tasks(&processes, &facts, super::loader::launchers(), |pkg| {
        super::apps::safety_for(state, pkg)
    });
    for app in &mut plan.apps {
        app.name = display_name(state, &app.package);
    }
    Ok(TaskSnapshot {
        metric,
        ram: super::health::ram_info(&sections[0], &sections[6]),
        apps: plan.apps,
        other: plan.other,
        home_handlers_read: home_handlers.is_some(),
    })
}

/// Packages named in a `pkg/Class[:pkg/Class…]` setting value. `null` and an
/// empty value name nothing.
fn component_packages(value: &str) -> Vec<String> {
    value
        .trim()
        .split(':')
        .filter_map(|component| component.split('/').next())
        .map(str::trim)
        .filter(|pkg| is_valid_package_name(pkg))
        .map(str::to_string)
        .collect()
}

fn display_name(state: &AppState, package: &str) -> Option<String> {
    state
        .known_names
        .get(package)
        .map(|known| known.name.clone())
        .or_else(|| state.app_lists.find(package).map(|e| e.name.clone()))
        .or_else(|| {
            let launchers = super::loader::launchers();
            launchers
                .custom
                .iter()
                .chain(launchers.stock.iter())
                .find(|e| e.package == package)
                .map(|e| e.name.clone())
        })
}

pub async fn stop_apps_impl(
    state: &AppState,
    serial: &str,
    packages: Vec<String>,
    mode: StopMode,
    settle: Duration,
) -> Result<StopReport, String> {
    let mut requested: Vec<String> = Vec::new();
    for pkg in packages {
        if !is_valid_package_name(&pkg) {
            return Err(format!("Refusing to act on invalid package name: {pkg:?}"));
        }
        if !requested.contains(&pkg) {
            requested.push(pkg);
        }
    }
    if requested.is_empty() {
        return Err("No apps were named to stop.".into());
    }
    if requested.len() > MAX_STOP_BATCH {
        return Err(format!(
            "Refusing to stop {} apps in one request (limit {MAX_STOP_BATCH}).",
            requested.len()
        ));
    }

    let before = read_snapshot(state, serial).await?;
    let mut results: Vec<StopResult> = Vec::with_capacity(requested.len());
    let mut targets: Vec<&TaskApp> = Vec::new();
    for pkg in &requested {
        let outcome = match before.apps.iter().find(|a| &a.package == pkg) {
            None => Some(KillOutcome::NotRunning),
            Some(app) => {
                let verdict = match mode {
                    StopMode::Kill => app.verdict.clone(),
                    StopMode::ForceStop => force_stop_verdict(app),
                };
                match verdict {
                    KillVerdict::Allowed => {
                        targets.push(app);
                        None
                    }
                    KillVerdict::Blocked { reason, detail } => {
                        Some(KillOutcome::Refused { reason, detail })
                    }
                }
            }
        };
        if let Some(outcome) = outcome {
            results.push(StopResult {
                package: pkg.clone(),
                outcome,
            });
        }
    }

    if targets.is_empty() {
        return Ok(StopReport {
            results,
            ram_before: before.ram.clone(),
            snapshot: Some(before),
            snapshot_error: None,
        });
    }

    let verb = match mode {
        StopMode::Kill => "am kill",
        StopMode::ForceStop => "am force-stop",
    };
    let commands: Vec<String> = targets
        .iter()
        .map(|app| format!("{verb} {}", app.package))
        .collect();
    let command_refs: Vec<&str> = commands.iter().map(String::as_str).collect();
    let adb = state.adb_snapshot().await;
    let sent = adb
        .shell(serial, &checked_batch_command(&command_refs))
        .await
        .map_err(|e| format!("{verb}: {e}"));
    let failures = match &sent {
        Ok(out) => command_failures(&out.stdout, targets.len()),
        Err(e) => vec![Some(e.clone()); targets.len()],
    };
    tracing::info!(serial, mode = ?mode, packages = ?targets.iter().map(|a| &a.package).collect::<Vec<_>>(), "task manager stop");

    tokio::time::sleep(settle).await;
    let after = read_snapshot(state, serial).await;

    for (app, failure) in targets.iter().zip(failures) {
        let outcome = match (failure, &after) {
            (Some(message), _) => KillOutcome::Failed { message },
            (None, Ok(after)) => kill_outcome(
                app,
                after.apps.iter().find(|a| a.package == app.package),
                mode == StopMode::ForceStop,
            ),
            (None, Err(_)) => KillOutcome::Unverified,
        };
        results.push(StopResult {
            package: app.package.clone(),
            outcome,
        });
    }
    results.sort_by_key(|r| requested.iter().position(|p| p == &r.package));

    let ram_before = before.ram.clone();
    let (snapshot, snapshot_error) = match after {
        Ok(after) => (Some(after), None),
        Err(e) => (None, Some(e)),
    };
    Ok(StopReport {
        results,
        ram_before,
        snapshot,
        snapshot_error,
    })
}

/// Per-command failure message from a checked batch of `am` calls, `None`
/// where the command succeeded. `am` exits 0 on some errors, so its output is
/// read too. A response cut short marks the missing commands as unconfirmed
/// failures rather than successes.
fn command_failures(output: &str, n: usize) -> Vec<Option<String>> {
    let mut sections = output.split(BATCH_SEPARATOR);
    (0..n)
        .map(|_| {
            let Some(section) = sections.next() else {
                return Some("The TV did not confirm this command.".into());
            };
            let Some((body, status)) = section.rsplit_once(BATCH_STATUS) else {
                return Some("The TV did not confirm this command.".into());
            };
            let body = body.trim();
            let failed = status.trim() != "0"
                || body.lines().any(|line| {
                    let line = line.trim();
                    line.starts_with("Error")
                        || line.starts_with("Failure")
                        || line.contains("Exception")
                        || line.contains("Unknown package")
                });
            failed.then(|| {
                if body.is_empty() {
                    format!("The command exited with status {}.", status.trim())
                } else {
                    body.to_string()
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::{state_with, MockAdb};
    use crate::engine::{BlockReason, Importance};

    const SHIELD: &str = "../../crates/core/tests/fixtures/devices/shield-tv-pro";

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/devices/shield-tv-pro")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{SHIELD}/{name}: {e}"))
    }

    fn setting(name: &str) -> String {
        fixture("settings-secure.txt")
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")).map(str::to_string))
            .unwrap_or_default()
    }

    /// The device's answer to a checked batch, one `(stdout, status)` per read.
    fn batch(sections: &[(&str, i32)]) -> String {
        sections
            .iter()
            .map(|(body, status)| format!("{body}\n{BATCH_STATUS}{status}\n"))
            .collect::<Vec<_>>()
            .join(&format!("{BATCH_SEPARATOR}\n"))
    }

    fn shield_read(meminfo: &str) -> String {
        batch(&[
            (meminfo, 0),
            (&fixture("pm-list-packages.txt"), 0),
            (&fixture("pm-list-packages-3.txt"), 0),
            (&setting("default_input_method"), 0),
            (&setting("enabled_accessibility_services"), 0),
            (&fixture("home-query-activities.txt"), 0),
            ("MemTotal: 3016708 kB", 0),
        ])
    }

    fn shield_state(mock: MockAdb) -> AppState {
        let lists = crate::commands::loader::load_embedded_app_lists().unwrap();
        let mut state = state_with(mock);
        state.app_lists = lists;
        state.with_known_names(crate::commands::loader::load_known_names())
    }

    fn app<'a>(snapshot: &'a TaskSnapshot, pkg: &str) -> &'a TaskApp {
        snapshot
            .apps
            .iter()
            .find(|a| a.package == pkg)
            .unwrap_or_else(|| panic!("{pkg} not listed"))
    }

    fn reason(app: &TaskApp) -> Option<BlockReason> {
        match &app.verdict {
            KillVerdict::Allowed => None,
            KillVerdict::Blocked { reason, .. } => Some(*reason),
        }
    }

    #[tokio::test]
    async fn real_shield_capture_offers_only_background_user_apps() {
        let mock = MockAdb::default().on_shell(
            "dumpsys meminfo",
            &shield_read(&fixture("dumpsys-meminfo.txt")),
        );
        let snapshot = read_snapshot(&shield_state(mock), "tv").await.unwrap();

        assert_eq!(snapshot.metric, MemoryMetric::Pss);
        assert!(snapshot.home_handlers_read);
        assert_eq!(snapshot.ram.total_mb, Some(2946));

        let plex = app(&snapshot, "com.plexapp.android");
        assert_eq!(plex.importance, Importance::Cached);
        assert!(plex.third_party);
        assert!(plex.verdict.is_allowed(), "{:?}", plex.verdict);
        assert_eq!(plex.background_mb, 250.4);

        assert_eq!(
            reason(app(&snapshot, "com.spocky.projengmenu")),
            Some(BlockReason::Launcher)
        );
        for protected in [
            "com.android.systemui",
            "com.google.android.inputmethod.latin",
        ] {
            assert_eq!(
                reason(app(&snapshot, protected)),
                Some(BlockReason::Protected)
            );
        }
        // Visible and not in the catalog as Safe: a system app, whatever its state.
        assert!(!app(&snapshot, "com.google.android.gms")
            .verdict
            .is_allowed());

        // A process that only looks like a package stays a process.
        assert!(snapshot.other.iter().any(|p| p.process == "media.codec"));
        assert!(snapshot.apps.iter().all(|a| a.package != "media.codec"));
        // Nothing the engine offers is a system app without a Safe review.
        for a in snapshot.apps.iter().filter(|a| a.verdict.is_allowed()) {
            assert!(
                a.third_party
                    || matches!(
                        crate::commands::apps::safety_for(
                            &shield_state(MockAdb::default()),
                            &a.package
                        ),
                        crate::engine::Safety::Safe { .. }
                    ),
                "{} offered without being user-installed or reviewed safe",
                a.package
            );
            assert!(a.processes.iter().any(|p| p.importance.is_background()));
        }
    }

    #[tokio::test]
    async fn unreadable_home_query_still_recognises_the_current_home_app() {
        let reply = shield_read(&fixture("dumpsys-meminfo.txt")).replace(
            &fixture("home-query-activities.txt"),
            "Unknown command: query-activities",
        );
        let mock = MockAdb::default().on_shell("dumpsys meminfo", &reply);
        let snapshot = read_snapshot(&shield_state(mock), "tv").await.unwrap();
        assert!(!snapshot.home_handlers_read);
        // Projectivy is in launchers.json, so it stays blocked without the query.
        assert_eq!(
            reason(app(&snapshot, "com.spocky.projengmenu")),
            Some(BlockReason::Launcher)
        );
    }

    #[tokio::test]
    async fn failed_required_read_is_an_error_not_an_empty_list() {
        let reply = batch(&[
            ("", 1),
            ("package:a.b", 0),
            ("", 0),
            ("null", 0),
            ("null", 0),
            ("", 1),
            ("", 0),
        ]);
        let mock = MockAdb::default().on_shell("dumpsys meminfo", &reply);
        assert!(read_snapshot(&state_with(mock), "tv").await.is_err());
    }

    #[test]
    fn setting_components_name_their_packages() {
        assert_eq!(
            component_packages("com.a.b/.Svc:com.c.d/com.c.d.Other\n"),
            ["com.a.b", "com.c.d"]
        );
        assert!(component_packages("null").is_empty());
        assert!(component_packages("").is_empty());
    }

    fn meminfo(rows: &[(&str, u32, u64, &str)]) -> String {
        let mut out = String::from("Total PSS by OOM adjustment:\n");
        let mut buckets: Vec<&str> = Vec::new();
        for row in rows {
            if !buckets.contains(&row.3) {
                buckets.push(row.3);
            }
        }
        for bucket in buckets {
            out.push_str(&format!("    1,000K: {bucket}\n"));
            for (process, pid, kb, _) in rows.iter().filter(|r| r.3 == bucket) {
                out.push_str(&format!("        {kb}K: {process} (pid {pid})\n"));
            }
        }
        out.push_str("\nTotal RAM: 2,000,000K\nFree RAM: 500,000K\n");
        out
    }

    fn simple_read(meminfo: &str) -> String {
        batch(&[
            (meminfo, 0),
            (
                "package:com.example.player\npackage:com.example.music\npackage:com.android.systemui",
                0,
            ),
            ("package:com.example.player\npackage:com.example.music", 0),
            ("null", 0),
            ("null", 0),
            ("    packageName=com.android.tv.settings", 0),
            ("", 0),
        ])
    }

    #[tokio::test]
    async fn stop_sends_only_what_passes_the_gate_on_a_fresh_read() {
        let before = meminfo(&[
            ("com.example.player", 10, 204800, "Cached"),
            ("com.example.music", 20, 102400, "Foreground"),
            ("com.android.systemui", 30, 51200, "Cached"),
        ]);
        let after = meminfo(&[("com.example.music", 20, 102400, "Foreground")]);
        let mock = MockAdb::default()
            .on_shell_seq(
                "dumpsys meminfo",
                &[&simple_read(&before), &simple_read(&after)],
            )
            .on_shell("am kill", &batch(&[("", 0)]));
        let log = mock.shell_log();
        let report = stop_apps_impl(
            &state_with(mock),
            "tv",
            vec![
                "com.example.player".into(),
                "com.example.music".into(),
                "com.android.systemui".into(),
                "com.example.gone".into(),
            ],
            StopMode::Kill,
            Duration::ZERO,
        )
        .await
        .unwrap();

        let sent: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.contains("am "))
            .cloned()
            .collect();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("am kill com.example.player"));
        assert!(!sent[0].contains("com.example.music"));
        assert!(!sent[0].contains("systemui"));

        let outcome = |pkg: &str| {
            &report
                .results
                .iter()
                .find(|r| r.package == pkg)
                .unwrap()
                .outcome
        };
        assert_eq!(
            outcome("com.example.player"),
            &KillOutcome::Stopped { freed_mb: 200.0 }
        );
        assert!(matches!(
            outcome("com.example.music"),
            KillOutcome::Refused {
                reason: BlockReason::InUse,
                ..
            }
        ));
        assert!(matches!(
            outcome("com.android.systemui"),
            KillOutcome::Refused {
                reason: BlockReason::Protected,
                ..
            }
        ));
        assert_eq!(outcome("com.example.gone"), &KillOutcome::NotRunning);
        assert_eq!(
            report
                .results
                .iter()
                .map(|r| r.package.as_str())
                .collect::<Vec<_>>(),
            [
                "com.example.player",
                "com.example.music",
                "com.android.systemui",
                "com.example.gone"
            ],
            "results come back in request order"
        );
        assert!(report
            .snapshot
            .unwrap()
            .apps
            .iter()
            .all(|a| a.package != "com.example.player"));
    }

    #[tokio::test]
    async fn force_stop_refuses_a_partly_in_use_app() {
        let before = meminfo(&[
            ("com.example.music", 20, 102400, "Perceptible"),
            ("com.example.music:cache", 21, 10240, "Cached"),
        ]);
        let mock = MockAdb::default().on_shell("dumpsys meminfo", &simple_read(&before));
        let log = mock.shell_log();
        let report = stop_apps_impl(
            &state_with(mock),
            "tv",
            vec!["com.example.music".into()],
            StopMode::ForceStop,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(log
            .lock()
            .unwrap()
            .iter()
            .all(|c| !c.contains("force-stop")));
        assert!(matches!(
            report.results[0].outcome,
            KillOutcome::Refused {
                reason: BlockReason::InUse,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn device_error_and_failed_reread_are_reported_honestly() {
        let before = meminfo(&[
            ("com.example.player", 10, 1024, "Cached"),
            ("com.example.music", 20, 1024, "Cached"),
        ]);
        let mock = MockAdb::default()
            .on_shell_seq("dumpsys meminfo", &[&simple_read(&before), "garbage"])
            .on_shell(
                "am kill",
                &batch(&[("", 0), ("java.lang.SecurityException: denied", 0)]),
            );
        let report = stop_apps_impl(
            &state_with(mock),
            "tv",
            vec!["com.example.player".into(), "com.example.music".into()],
            StopMode::Kill,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(report.results[0].outcome, KillOutcome::Unverified);
        assert!(matches!(
            &report.results[1].outcome,
            KillOutcome::Failed { message } if message.contains("SecurityException")
        ));
        assert!(report.snapshot.is_none());
        assert!(report.snapshot_error.is_some());
    }

    #[tokio::test]
    async fn malformed_requests_never_reach_the_device() {
        for packages in [
            vec![],
            vec!["com.example;reboot".to_string()],
            (0..=MAX_STOP_BATCH)
                .map(|i| format!("com.example.a{i}"))
                .collect(),
        ] {
            let mock = MockAdb::default();
            let log = mock.shell_log();
            assert!(stop_apps_impl(
                &state_with(mock),
                "tv",
                packages,
                StopMode::Kill,
                Duration::ZERO
            )
            .await
            .is_err());
            assert!(log.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn an_error_line_inside_the_dump_does_not_sink_the_read() {
        let dump = format!(
            "Error: dumping process 4242 failed\n{}",
            meminfo(&[("com.example.player", 10, 1024, "Cached")])
        );
        let mock = MockAdb::default().on_shell("dumpsys meminfo", &simple_read(&dump));
        let snapshot = read_snapshot(&state_with(mock), "tv").await.unwrap();
        assert_eq!(snapshot.apps[0].package, "com.example.player");

        let mock = MockAdb::default().on_shell("dumpsys meminfo", &simple_read(""));
        assert!(read_snapshot(&state_with(mock), "tv").await.is_err());
    }
}
