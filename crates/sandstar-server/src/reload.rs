//! Configuration reload.
//!
//! Re-reads database.zinc and tables.csv from disk, updating channel
//! metadata and the poll list. Does NOT add or remove physical channels
//! (that requires a restart with points.csv changes).
//!
//! ## Granular reload (7.0b)
//!
//! The reload is diff-based: only changed channels are updated, and the
//! poll list is surgically patched (add missing, remove stale) instead of
//! cleared and rebuilt. This preserves runtime state (last_value,
//! unchanged_count, consecutive_fail_count) for unchanged channels, avoiding
//! disruption to watch subscriptions and poll cycles.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use sandstar_engine::conversion::auto_detect::auto_detect_tag;
use sandstar_engine::table::{TableStore, MISSING_TABLE};
use sandstar_engine::Engine;
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use tracing::{error, info, warn};

use crate::control::{ControlReload, ControlRunner};
use crate::loader;

/// The control runner a reload may replace, with the settings that decide
/// whether and from where `control.toml` is read.
pub struct ControlTarget<'a> {
    pub runner: &'a mut ControlRunner,
    /// `--no-control`: the control engine is off; the file is not read.
    pub no_control: bool,
    /// `--control-config`; `None` means `control.toml` in the config directory.
    pub path: Option<&'a Path>,
}

/// One reload for every entry point (SIGHUP, REST, IPC): the configuration
/// directory first, then `control.toml`.
///
/// - The configuration fails: `Err`, and `control.toml` is not read.
/// - `control.toml` fails: `Err` that says so and carries the summary; the
///   tables and channels are reloaded, the previous control loops keep running.
/// - Both fine: the summary followed by what happened to the control loops.
///
/// `control` is `None` where there is no runner to reload; the reply says so.
pub fn reload_all<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    config_dir: &Path,
    read_only: bool,
    control: Option<&mut ControlTarget<'_>>,
) -> Result<String, String> {
    let summary = reload_config(engine, config_dir).map_err(|e| e.to_string())?;
    let Some(control) = control else {
        return Ok(format!("{summary}; control: not reloaded (no runner)"));
    };
    if control.no_control || read_only {
        return Ok(format!("{summary}; control: disabled"));
    }
    let path = match control.path {
        Some(path) => path.to_path_buf(),
        None => config_dir.join("control.toml"),
    };
    // Only "not found" counts as a missing file; any other failure to reach
    // it is an error, like a failure to read it.
    let missing = match std::fs::metadata(&path) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            return Err(control_failure(
                &format!("cannot access {}: {}", path.display(), e),
                &summary,
                control.runner,
            ));
        }
    };
    if missing {
        if !control.runner.is_empty() {
            warn!(
                path = %path.display(),
                loops = control.runner.loop_count(),
                components = control.runner.component_count(),
                "control file is gone; the loaded control loops keep running until restart"
            );
            return Ok(format!(
                "{summary}; control: file missing, loaded loops kept"
            ));
        }
        return Ok(format!("{summary}; control: no control file"));
    }
    match control.runner.reload(&path) {
        Ok(ControlReload::Unchanged) => Ok(format!("{summary}; control: unchanged")),
        Ok(ControlReload::Replaced { loops, components }) => {
            info!(loops, components, "control config reloaded");
            Ok(format!(
                "{summary}; control: reloaded {loops} loops, {components} components"
            ))
        }
        Err(e) => Err(control_failure(&e, &summary, control.runner)),
    }
}

/// The error for a control file that could not be reloaded: the cause, and
/// what the reload did do.
fn control_failure(cause: &str, summary: &ReloadSummary, runner: &ControlRunner) -> String {
    error!(error = %cause, "failed to reload control config (keeping previous)");
    let kept = if runner.is_empty() {
        "no control loops are loaded"
    } else {
        "previous control loops kept"
    };
    format!(
        "control config reload failed: {cause} (tables and channels reloaded: {summary}; {kept})"
    )
}

/// Summary of what changed during a config reload.
#[derive(Debug)]
pub struct ReloadSummary {
    pub channels_updated: usize,
    pub channels_added: usize,
    pub channels_removed: usize,
    pub channels_unchanged: usize,
    pub tables_reloaded: usize,
    pub polls_added: usize,
    pub polls_removed: usize,
    pub polls_after: usize,
    pub errors: Vec<String>,
}

impl fmt::Display for ReloadSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "channels: {} added, {} removed, {} modified, {} unchanged | \
             tables={} | polls: {} added, {} removed, {} total | errors={}",
            self.channels_added,
            self.channels_removed,
            self.channels_updated,
            self.channels_unchanged,
            self.tables_reloaded,
            self.polls_added,
            self.polls_removed,
            self.polls_after,
            self.errors.len()
        )
    }
}

/// Reload configuration from disk (granular, diff-based).
///
/// Re-reads database.zinc and tables.csv, updating only changed channels
/// and surgically patching the poll list. Preserves runtime state
/// (priority_array, current value, poll counters) for unchanged channels.
///
/// Does NOT add or remove physical channels from points.csv.
pub fn reload_config<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    config_dir: &Path,
) -> Result<ReloadSummary, Box<dyn std::error::Error>> {
    let mut summary = ReloadSummary {
        channels_updated: 0,
        channels_added: 0,
        channels_removed: 0,
        channels_unchanged: 0,
        tables_reloaded: 0,
        polls_added: 0,
        polls_removed: 0,
        polls_after: 0,
        errors: Vec::new(),
    };

    // A reload that could not read what it was asked to read is an error
    // for the caller, not a line in the log (R-62).
    //
    // It is also all or nothing: when a file cannot be read, the engine
    // keeps the tables, channels and poll list it had.
    if !config_dir.is_dir() {
        // Not "not found": the REST layer would turn that into a 404.
        return Err(format!("config directory is missing: {}", config_dir.display()).into());
    }

    // 1. Reload tables (tables.csv + data files)
    let tables_csv = config_dir.join("tables.csv");
    let database_path = config_dir.join("database.zinc");
    if !tables_csv.exists() && !database_path.exists() {
        // Nothing was read, so "reloaded" would be untrue (R-63).
        return Err(format!(
            "nothing to reload: no tables.csv and no database.zinc in {}",
            config_dir.display()
        )
        .into());
    }
    let table_dir = [
        config_dir.join("../usr/local/config"),
        config_dir.join("config"),
        config_dir.to_path_buf(),
    ]
    .into_iter()
    .find(|p| p.exists())
    .unwrap_or_else(|| config_dir.to_path_buf());

    // Loaded on the side: a file that cannot be read must not leave the
    // engine without tables. The previous store is kept until the database
    // step is through, so a failure there can put it back.
    let mut previous_tables = None;
    let mut previous_bindings = Vec::new();
    if tables_csv.exists() {
        let mut fresh = TableStore::new();
        match loader::load_tables(&mut fresh, &tables_csv, &table_dir) {
            // A table set with holes is not a reload (R-63): the channels
            // of a skipped table would lose their conversion.
            Ok(report) if !report.skipped.is_empty() => {
                let msg = format!(
                    "failed to reload tables from {} (nothing changed): {} table(s) not loaded: {}",
                    tables_csv.display(),
                    report.skipped.len(),
                    report.skipped_summary()
                );
                error!("{}", msg);
                return Err(msg.into());
            }
            // A tables.csv with no table in it is a cut-off or emptied
            // file, not a request to drop every table.
            Ok(report) if report.loaded == 0 => {
                let msg = format!(
                    "failed to reload tables from {} (nothing changed): the file lists no table",
                    tables_csv.display()
                );
                error!("{}", msg);
                return Err(msg.into());
            }
            Ok(report) => {
                let previous = std::mem::replace(&mut engine.tables, fresh);
                previous_bindings = rebind_tables(engine, &previous);
                previous_tables = Some(previous);
                summary.tables_reloaded = report.loaded;
                info!(count = report.loaded, "tables reloaded");
            }
            Err(e) => {
                let msg = format!(
                    "failed to reload tables from {} (nothing changed): {}",
                    tables_csv.display(),
                    e
                );
                error!("{}", msg);
                return Err(msg.into());
            }
        }
    }

    // 2. Reload database.zinc (granular diff-based)
    if database_path.exists() {
        match loader::load_database_granular(engine, &database_path) {
            Ok(result) => {
                summary.channels_updated = result.modified;
                summary.channels_added = result.added.len();
                summary.channels_removed = result.removed.len();
                summary.channels_unchanged = result.unchanged;

                // Forward any per-channel warnings
                for w in &result.warnings {
                    warn!("{}", w);
                }
                summary.errors.extend(result.warnings);

                // 3. Diff the poll list: add missing, remove stale
                let current_poll_ids: HashSet<u32> =
                    engine.polls.iter().map(|(&id, _)| id).collect();

                // Add polls that should exist but don't
                for &id in &result.expected_polls {
                    if !current_poll_ids.contains(&id) {
                        let _ = engine.polls.add(id);
                        summary.polls_added += 1;
                    }
                }

                // Remove polls that exist but shouldn't
                let stale: Vec<u32> = current_poll_ids
                    .iter()
                    .filter(|id| !result.expected_polls.contains(id))
                    .copied()
                    .collect();
                for id in stale {
                    let _ = engine.polls.remove(id);
                    summary.polls_removed += 1;
                }

                info!(
                    added = summary.channels_added,
                    removed = summary.channels_removed,
                    modified = summary.channels_updated,
                    unchanged = summary.channels_unchanged,
                    polls_added = summary.polls_added,
                    polls_removed = summary.polls_removed,
                    "granular reload: database.zinc"
                );
            }
            Err(e) => {
                // The loader fails before it touches a channel (read and
                // parse come first), so the channels and the poll list are
                // as they were; the tables go back too.
                if let Some(previous) = previous_tables {
                    engine.tables = previous;
                    for (id, index) in previous_bindings {
                        if let Some(ch) = engine.channels.get_mut(id) {
                            ch.conv.table_index = index;
                        }
                    }
                }
                let msg = format!(
                    "failed to reload {} (nothing changed): {}",
                    database_path.display(),
                    e
                );
                error!("{}", msg);
                return Err(msg.into());
            }
        }
    } else {
        // No database.zinc — rebuild polls from all input channels
        let polls_snapshot = engine.polls.snapshot();
        engine.polls.clear();
        let fallback_count = loader::setup_polls(engine);
        if fallback_count == 0 && engine.polls.count() == 0 && !polls_snapshot.is_empty() {
            error!(
                prev_polls = polls_snapshot.len(),
                "setup_polls produced 0 polls — rolling back to previous poll list"
            );
            engine.polls.restore(polls_snapshot);
        }
    }

    summary.polls_after = engine.polls.count();

    if summary.errors.is_empty() {
        info!(%summary, "config reload complete");
    } else {
        warn!(%summary, "config reload completed with warnings");
    }

    Ok(summary)
}

/// Point every channel's table index at the same table in the new store.
///
/// A channel holds an index into the table store, and a reload can change
/// the order of the tables. The table is found again by its tag. A channel
/// whose table is gone gets [`MISSING_TABLE`]: it reads as fault and
/// refuses writes until its configuration names a table that exists.
///
/// Returns the indices that were changed, so a failed reload can put them
/// back together with the previous tables.
fn rebind_tables<H: HalRead + HalWrite + HalDiagnostics>(
    engine: &mut Engine<H>,
    previous: &TableStore,
) -> Vec<(u32, Option<usize>)> {
    let mut changed = Vec::new();
    let tables = &engine.tables;
    for (&id, ch) in engine.channels.iter_mut() {
        let Some(old_index) = ch.conv.table_index else {
            continue;
        };
        if old_index == MISSING_TABLE {
            // Marked by auto-detection, and the table its channel number
            // stands for is loaded now: take the mark off, the next read
            // binds the table. Without this a channel that no database
            // row rebuilds would stay in fault until a restart.
            let table_is_back =
                auto_detect_tag(id, &ch.conv).is_some_and(|tag| tables.find_by_tag(tag).is_some());
            if table_is_back {
                changed.push((id, Some(MISSING_TABLE)));
                ch.conv.table_index = None;
            }
            continue;
        }
        let new_index = previous
            .get(old_index)
            .and_then(|table| tables.find_by_tag_and_unit(&table.tag, &table.unit_type))
            .unwrap_or(MISSING_TABLE);
        if new_index == MISSING_TABLE {
            warn!(
                channel = id,
                "conversion table is gone after reload: channel reads as fault and rejects writes"
            );
        }
        if new_index != old_index {
            changed.push((id, Some(old_index)));
            ch.conv.table_index = Some(new_index);
        }
    }
    changed
}

/// Fixtures for the reload tests here and at the three entry points.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    /// A config directory `reload_config` accepts: one table.
    pub(crate) fn write_config_dir(dir: &Path) {
        std::fs::write(
            dir.join("tables.csv"),
            "name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag\n\
             T,Test,t.txt,temp,-40,303,-40,150,233,423,testTherm\n",
        )
        .unwrap();
        std::fs::write(dir.join("t.txt"), "100\n200\n300\n").unwrap();
    }

    /// A control file with one loop.
    pub(crate) fn control_toml(kp: f64) -> String {
        format!(
            "[[loop]]\nname = \"zone\"\nfeedback_channel = 4\nsetpoint_channel = 75\n\
             output_channels = [35]\n\n[loop.pid]\nkp = {kp:?}\n"
        )
    }

    pub(crate) const BAD_CONTROL_TOML: &str = "[[loop]]\nname = ";
}

#[cfg(test)]
mod reload_all_tests {
    use super::test_support::*;
    use super::*;
    use sandstar_hal::mock::MockHal;
    use tempfile::TempDir;

    fn engine() -> Engine<MockHal> {
        Engine::new(MockHal::new())
    }

    fn target(runner: &mut ControlRunner) -> ControlTarget<'_> {
        ControlTarget {
            runner,
            no_control: false,
            path: None,
        }
    }

    #[test]
    fn loads_a_new_control_file_and_leaves_an_unchanged_one_alone() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        std::fs::write(dir.path().join("control.toml"), control_toml(1.0)).unwrap();
        let mut engine = engine();
        let mut runner = ControlRunner::new();

        let first = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap();
        assert!(
            first.ends_with("; control: reloaded 1 loops, 0 components"),
            "{first}"
        );
        assert_eq!(engine.tables.count(), 1);
        assert_eq!(runner.loop_count(), 1);

        let second = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap();
        assert!(second.ends_with("; control: unchanged"), "{second}");
        std::fs::write(
            dir.path().join("control.toml"),
            control_toml(1.0) + "# note\n",
        )
        .unwrap();
        let third = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap();
        assert!(
            third.ends_with("; control: reloaded 1 loops, 0 components"),
            "{third}"
        );
    }

    #[test]
    fn a_failed_configuration_reload_does_not_read_the_control_file() {
        let dir = TempDir::new().unwrap();
        // Unreadable tables.csv: a directory where the file should be.
        std::fs::create_dir(dir.path().join("tables.csv")).unwrap();
        std::fs::write(dir.path().join("control.toml"), control_toml(1.0)).unwrap();
        let mut engine = engine();
        let mut runner = ControlRunner::new();

        let err = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap_err();
        assert!(!err.contains("control"), "{err}");
        assert!(runner.is_empty());
    }

    #[test]
    fn a_bad_control_file_is_an_error_that_keeps_the_loops_and_names_both_results() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        let control = dir.path().join("control.toml");
        std::fs::write(&control, control_toml(1.0)).unwrap();
        let mut engine = engine();
        let mut runner = ControlRunner::load(&control).unwrap();

        std::fs::write(&control, BAD_CONTROL_TOML).unwrap();
        let err = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap_err();
        assert!(err.starts_with("control config reload failed: "), "{err}");
        assert!(err.contains("tables and channels reloaded"), "{err}");
        assert!(err.contains("previous control loops kept"), "{err}");
        assert_eq!(engine.tables.count(), 1);
        assert_eq!(runner.loop_count(), 1);
    }

    #[test]
    fn no_control_and_read_only_leave_the_control_file_unread() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        std::fs::write(dir.path().join("control.toml"), control_toml(1.0)).unwrap();
        let mut engine = engine();

        let mut runner = ControlRunner::new();
        let mut off = target(&mut runner);
        off.no_control = true;
        let text = reload_all(&mut engine, dir.path(), false, Some(&mut off)).unwrap();
        assert!(text.ends_with("; control: disabled"), "{text}");
        assert!(runner.is_empty());

        let text = reload_all(
            &mut engine,
            dir.path(),
            true,
            Some(&mut target(&mut runner)),
        )
        .unwrap();
        assert!(text.ends_with("; control: disabled"), "{text}");
        assert!(runner.is_empty());
    }

    #[test]
    fn a_missing_control_file_keeps_what_is_loaded() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        let control = dir.path().join("control.toml");
        let mut engine = engine();

        let mut runner = ControlRunner::new();
        let text = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap();
        assert!(text.ends_with("; control: no control file"), "{text}");

        std::fs::write(&control, control_toml(1.0)).unwrap();
        let mut runner = ControlRunner::load(&control).unwrap();
        std::fs::remove_file(&control).unwrap();
        let text = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap();
        assert!(
            text.ends_with("; control: file missing, loaded loops kept"),
            "{text}"
        );
        assert_eq!(runner.loop_count(), 1);
    }

    #[test]
    fn the_control_config_path_is_used_instead_of_the_directory_file() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        std::fs::write(dir.path().join("control.toml"), BAD_CONTROL_TOML).unwrap();
        let elsewhere = TempDir::new().unwrap();
        let path = elsewhere.path().join("my-control.toml");
        std::fs::write(&path, control_toml(1.0)).unwrap();
        let mut engine = engine();
        let mut runner = ControlRunner::new();

        let mut control = target(&mut runner);
        control.path = Some(&path);
        let text = reload_all(&mut engine, dir.path(), false, Some(&mut control)).unwrap();
        assert!(
            text.ends_with("; control: reloaded 1 loops, 0 components"),
            "{text}"
        );
    }

    #[test]
    fn without_a_runner_only_the_configuration_is_reloaded() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        std::fs::write(dir.path().join("control.toml"), BAD_CONTROL_TOML).unwrap();
        let mut engine = engine();
        let text = reload_all(&mut engine, dir.path(), false, None).unwrap();
        assert!(
            text.ends_with("; control: not reloaded (no runner)"),
            "{text}"
        );
        assert_eq!(engine.tables.count(), 1);
    }

    /// A control file that is there but cannot be read is an error, not a
    /// missing file.
    #[test]
    fn an_unreadable_control_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        write_config_dir(dir.path());
        let control = dir.path().join("control.toml");
        std::fs::write(&control, control_toml(1.0)).unwrap();
        let mut engine = engine();
        let mut runner = ControlRunner::load(&control).unwrap();

        // A directory where the file should be.
        std::fs::remove_file(&control).unwrap();
        std::fs::create_dir(&control).unwrap();
        let err = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut runner)),
        )
        .unwrap_err();
        assert!(err.starts_with("control config reload failed: "), "{err}");
        assert!(err.contains("previous control loops kept"), "{err}");
        assert_eq!(runner.loop_count(), 1);

        // With nothing loaded the reply does not claim loops were kept.
        let mut empty = ControlRunner::new();
        let err = reload_all(
            &mut engine,
            dir.path(),
            false,
            Some(&mut target(&mut empty)),
        )
        .unwrap_err();
        assert!(err.contains("no control loops are loaded"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandstar_engine::channel::{Channel, ChannelDirection, ChannelType};
    use sandstar_engine::value::ValueConv;
    use sandstar_engine::{EngineStatus, EngineValue, ValueFlags};
    use sandstar_hal::mock::MockHal;
    use std::fs;
    use tempfile::TempDir;

    fn make_engine_with_channel(id: u32) -> Engine<MockHal> {
        let hal = MockHal::new();
        let mut engine = Engine::new(hal);
        let ch = Channel::new(
            id,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            0,
            false,
            ValueConv::default(),
            "test",
        );
        engine.channels.add(ch).unwrap();
        engine
    }

    #[test]
    fn test_reload_no_database_falls_back_to_setup_polls() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);

        // Enable the channel so setup_polls picks it up
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Tables only: a directory with neither file is an error (R-63).
        write_good_tables(dir.path());

        let result = reload_config(&mut engine, dir.path()).unwrap();
        // setup_polls should add the enabled input channel
        assert_eq!(result.polls_after, 1);
        assert_eq!(result.tables_reloaded, 1);
    }

    #[test]
    fn test_reload_with_tables() {
        let dir = TempDir::new().unwrap();

        // Create a tables.csv matching the 11-column format load_tables() expects:
        // name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag
        fs::write(
            dir.path().join("tables.csv"),
            "name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag\n\
             test_table,Test,test_data.txt,temp,-40,303,-40,150,233,423,testTherm\n",
        )
        .unwrap();
        // Table data needs at least 2 values
        fs::write(dir.path().join("test_data.txt"), "100\n200\n300\n").unwrap();

        let mut engine = make_engine_with_channel(1100);
        let result = reload_config(&mut engine, dir.path()).unwrap();
        assert!(result.tables_reloaded > 0 || !result.errors.is_empty());
    }

    #[test]
    fn test_reload_bad_database_rolls_back_polls() {
        let dir = TempDir::new().unwrap();

        // Create a malformed database.zinc that will fail to parse
        fs::write(
            dir.path().join("database.zinc"),
            "not valid zinc at all {{{\n",
        )
        .unwrap();

        let mut engine = make_engine_with_channel(1100);

        // Enable the channel so setup_polls will pick it up as fallback
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Pre-populate polls so there's something to roll back to
        engine.polls.add(1100).unwrap();
        assert_eq!(engine.polls.count(), 1);

        // A second channel that is enabled but not polled: a rebuilt list
        // would pick it up, the kept list does not.
        let extra = Channel::new(
            1200,
            ChannelType::Analog,
            ChannelDirection::In,
            0,
            1,
            false,
            ValueConv::default(),
            "extra",
        );
        engine.channels.add(extra).unwrap();
        engine.channels.get_mut(1200).unwrap().enabled = true;

        // The caller is told (R-62)...
        let err = reload_config(&mut engine, dir.path()).unwrap_err();
        assert!(err.to_string().contains("database.zinc"), "{err}");

        // ...and the poll list is the one it was.
        let polled: Vec<u32> = engine.polls.iter().map(|(&id, _)| id).collect();
        assert_eq!(polled, vec![1100]);
        assert_eq!(engine.channels.count(), 2);
    }

    /// Good tables and a bad database: the new tables do not stay.
    #[test]
    fn a_bad_database_puts_the_previous_tables_back() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);
        assert_eq!(engine.tables.count(), 0);

        write_good_tables(dir.path());
        fs::write(dir.path().join("database.zinc"), "not zinc {{{\n").unwrap();

        let err = reload_config(&mut engine, dir.path()).unwrap_err();
        assert!(err.to_string().contains("database.zinc"), "{err}");
        assert_eq!(
            engine.tables.count(),
            0,
            "new tables stayed after a failed reload"
        );
    }

    /// Bad tables and a database file: the database is not applied.
    #[test]
    fn bad_tables_stop_the_reload_before_the_database() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("tables.csv")).unwrap();
        // A grid that parses and names no channel: applied, it would remove
        // nothing here, so the poll list is the witness.
        fs::write(
            dir.path().join("database.zinc"),
            "ver:\"3.0\"\nid,channel\n",
        )
        .unwrap();
        let mut engine = make_engine_with_channel(1100);
        engine.channels.get_mut(1100).unwrap().enabled = true;
        engine.polls.add(1100).unwrap();

        let err = reload_config(&mut engine, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("tables"), "{err}");
        assert!(!err.contains("database.zinc"), "{err}");
        assert_eq!(engine.polls.count(), 1);
        assert_eq!(engine.channels.count(), 1);
    }

    #[test]
    fn reload_with_missing_config_dir_is_an_error_and_keeps_everything() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("no-such-dir");
        let mut engine = make_engine_with_channel(1100);
        engine.polls.add(1100).unwrap();

        let err = reload_config(&mut engine, &missing).unwrap_err();
        assert!(
            err.to_string().contains("config directory is missing"),
            "{err}"
        );
        assert_eq!(engine.channels.count(), 1);
        assert_eq!(engine.polls.count(), 1);
    }

    /// A path that is a file, not a directory, is refused the same way.
    #[test]
    fn reload_with_a_file_as_config_dir_is_an_error() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("database.zinc");
        fs::write(&file, "x").unwrap();
        let mut engine = make_engine_with_channel(1100);
        assert!(reload_config(&mut engine, &file).is_err());
    }

    fn write_good_tables(dir: &std::path::Path) {
        fs::write(
            dir.join("tables.csv"),
            "name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag\n\
             test_table,Test,test_data.txt,temp,-40,303,-40,150,233,423,testTherm\n",
        )
        .unwrap();
        fs::write(dir.join("test_data.txt"), "100\n200\n300\n").unwrap();
    }

    #[test]
    fn reload_that_cannot_read_tables_is_an_error_and_keeps_the_old_tables() {
        let dir = TempDir::new().unwrap();
        write_good_tables(dir.path());
        let mut engine = make_engine_with_channel(1100);
        let first = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(first.tables_reloaded, 1);
        assert_eq!(engine.tables.count(), 1);

        // tables.csv is now something that cannot be read as a file.
        fs::remove_file(dir.path().join("tables.csv")).unwrap();
        fs::create_dir(dir.path().join("tables.csv")).unwrap();

        let err = reload_config(&mut engine, dir.path()).unwrap_err();
        assert!(err.to_string().contains("tables"), "{err}");
        assert_eq!(engine.tables.count(), 1, "old tables were dropped");
    }

    #[test]
    fn a_good_reload_replaces_the_tables() {
        let dir = TempDir::new().unwrap();
        write_good_tables(dir.path());
        let mut engine = make_engine_with_channel(1100);
        reload_config(&mut engine, dir.path()).unwrap();
        // A second reload does not pile the same table up twice.
        let again = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(again.tables_reloaded, 1);
        assert_eq!(engine.tables.count(), 1);
    }

    #[test]
    fn test_poll_snapshot_restore() {
        use sandstar_engine::poll::PollStore;

        let mut store = PollStore::new();
        store.add(1100).unwrap();
        store.add(1200).unwrap();

        // Take snapshot
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);

        // Clear
        store.clear();
        assert_eq!(store.count(), 0);

        // Restore
        store.restore(snap);
        assert_eq!(store.count(), 2);
        assert!(store.contains(1100));
        assert!(store.contains(1200));
    }

    // ====================================================================
    // Granular reload tests (Phase 7.0b)
    // ====================================================================

    /// Write a minimal database.zinc file for the given channels.
    /// Each entry is (id, label, is_virtual, has_cur_marker).
    fn write_database_zinc(dir: &TempDir, channels: &[(u32, &str, bool, bool)]) {
        let mut cols: Vec<&str> = vec!["channel", "enabled", "navName"];
        let has_virtual = channels.iter().any(|(_, _, v, _)| *v);
        let has_cur = channels.iter().any(|(_, _, _, c)| *c);
        if has_virtual {
            cols.push("virtualChannel");
            cols.push("analog");
        }
        if has_cur {
            cols.push("cur");
        }

        let mut content = String::from("ver:\"3.0\"\n");
        content.push_str(&cols.join(","));
        content.push('\n');

        for (id, label, is_virtual, has_cur_marker) in channels {
            // channel
            content.push_str(&id.to_string());
            content.push(',');
            // enabled
            content.push_str("M,");
            // navName
            content.push_str(&format!("\"{}\"", label));
            // virtualChannel (if column exists)
            if has_virtual {
                content.push(',');
                if *is_virtual {
                    content.push('M');
                }
                content.push(',');
                if *is_virtual {
                    content.push('M');
                } // analog marker for virtual
            }
            // cur (if column exists)
            if has_cur {
                content.push(',');
                if *has_cur_marker {
                    content.push('M');
                }
            }
            content.push('\n');
        }

        fs::write(dir.path().join("database.zinc"), content).unwrap();
    }

    #[test]
    fn test_granular_reload_adds_virtual_channel() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Initial load: just physical channel 1100
        write_database_zinc(&dir, &[(1100, "temp", false, true)]);
        let _result = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(engine.channels.count(), 1);
        assert!(engine.polls.contains(1100));

        // Reload with an extra virtual channel
        write_database_zinc(
            &dir,
            &[
                (1100, "temp", false, true),
                (3000, "virtual_temp", true, false),
            ],
        );
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(
            result.channels_added, 1,
            "should have added 1 virtual channel"
        );
        assert!(
            engine.channels.contains(3000),
            "virtual channel 3000 should exist"
        );
        assert!(
            engine.polls.contains(3000),
            "virtual channel should be polled"
        );
        assert!(
            engine.polls.contains(1100),
            "original poll should still exist"
        );

        let ch = engine.channels.get(3000).unwrap();
        assert!(ch.channel_type.is_virtual());
        assert_eq!(ch.label, "virtual_temp");
    }

    #[test]
    fn test_granular_reload_removes_virtual_channel() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Initial load: physical 1100 + virtual 3000
        write_database_zinc(
            &dir,
            &[
                (1100, "temp", false, true),
                (3000, "virtual_temp", true, false),
            ],
        );
        reload_config(&mut engine, dir.path()).unwrap();
        assert!(engine.channels.contains(3000));

        // Reload without virtual channel
        write_database_zinc(&dir, &[(1100, "temp", false, true)]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(
            result.channels_removed, 1,
            "should have removed 1 virtual channel"
        );
        assert!(
            !engine.channels.contains(3000),
            "virtual channel 3000 should be gone"
        );
        assert!(
            !engine.polls.contains(3000),
            "removed channel should not be polled"
        );
        assert!(
            engine.polls.contains(1100),
            "original poll should still exist"
        );
    }

    #[test]
    fn test_granular_reload_preserves_runtime_state() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Initial load
        write_database_zinc(&dir, &[(1100, "temp", false, true)]);
        reload_config(&mut engine, dir.path()).unwrap();

        // Simulate runtime state: set priority array + current value + poll state
        {
            let ch = engine.channels.get_mut(1100).unwrap();
            ch.priority_array = Some(sandstar_engine::priority::PriorityArray::default());
            ch.priority_array
                .as_mut()
                .unwrap()
                .set_level(8, Some(72.0), "test", 0.0);
            ch.value = EngineValue {
                status: EngineStatus::Ok,
                cur: 72.5,
                raw: 2048.0,
                flags: ValueFlags::CUR,
                trigger: false,
            };
        }

        // Set poll runtime state
        {
            let poll_value = EngineValue {
                status: EngineStatus::Ok,
                cur: 72.5,
                raw: 2048.0,
                flags: ValueFlags::CUR,
                trigger: false,
            };
            engine.polls.record_value(1100, &poll_value);
            engine.polls.record_value(1100, &poll_value); // unchanged_count = 1
        }

        // Reload with modified label (config change)
        write_database_zinc(&dir, &[(1100, "temperature_sensor", false, true)]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(
            result.channels_updated, 1,
            "label changed, should be modified"
        );

        // Label should be updated
        let ch = engine.channels.get(1100).unwrap();
        assert_eq!(ch.label, "temperature_sensor");

        // Priority array should be preserved (update_metadata doesn't touch it)
        assert!(
            ch.priority_array.is_some(),
            "priority array must survive reload"
        );
        let (eff, _level) = ch.priority_array.as_ref().unwrap().effective();
        assert_eq!(
            eff,
            Some(72.0),
            "priority write at level 8 must survive reload"
        );

        // Current value should be preserved
        assert_eq!(ch.value.cur, 72.5, "current value must survive reload");

        // Poll runtime state should be preserved (polls not cleared)
        let poll_item = engine.polls.get(1100).unwrap();
        assert_eq!(
            poll_item.unchanged_count, 1,
            "poll unchanged_count must survive granular reload"
        );
    }

    #[test]
    fn test_granular_reload_unchanged_not_disrupted() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Initial load
        write_database_zinc(&dir, &[(1100, "temp", false, true)]);
        reload_config(&mut engine, dir.path()).unwrap();

        // Set poll runtime state
        {
            let poll_value = EngineValue {
                status: EngineStatus::Ok,
                cur: 72.5,
                raw: 2048.0,
                flags: ValueFlags::CUR,
                trigger: false,
            };
            engine.polls.record_value(1100, &poll_value);
            engine.polls.record_value(1100, &poll_value); // unchanged_count = 1
            engine.polls.record_value(1100, &poll_value); // unchanged_count = 2
        }

        // Reload with identical config — nothing should change
        write_database_zinc(&dir, &[(1100, "temp", false, true)]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(result.channels_updated, 0, "nothing modified");
        assert_eq!(
            result.channels_unchanged, 1,
            "channel should be marked unchanged"
        );
        assert_eq!(result.polls_added, 0, "no new polls");
        assert_eq!(result.polls_removed, 0, "no removed polls");

        // Poll runtime state must be perfectly preserved
        let poll_item = engine.polls.get(1100).unwrap();
        assert_eq!(
            poll_item.unchanged_count, 2,
            "poll unchanged_count must survive unchanged reload"
        );
    }

    #[test]
    fn test_granular_reload_updates_existing_virtual() {
        let dir = TempDir::new().unwrap();
        let mut engine = Engine::<MockHal>::new(MockHal::new());

        // Load a virtual channel
        write_database_zinc(&dir, &[(3000, "old_label", true, false)]);
        reload_config(&mut engine, dir.path()).unwrap();
        assert!(engine.channels.contains(3000));
        assert_eq!(engine.channels.get(3000).unwrap().label, "old_label");

        // Reload with updated label — should update in-place, not fail
        write_database_zinc(&dir, &[(3000, "new_label", true, false)]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(result.channels_updated, 1, "virtual channel label changed");
        assert_eq!(result.channels_added, 0, "no new channels");
        assert_eq!(engine.channels.get(3000).unwrap().label, "new_label");
    }

    #[test]
    fn test_granular_reload_poll_diff() {
        let dir = TempDir::new().unwrap();

        // Start with physical channels 1100 and 1200
        let mut engine = Engine::<MockHal>::new(MockHal::new());
        for id in [1100, 1200] {
            let ch = Channel::new(
                id,
                ChannelType::Analog,
                ChannelDirection::In,
                0,
                0,
                false,
                ValueConv::default(),
                "test",
            );
            engine.channels.add(ch).unwrap();
        }

        // Load: both channels polled
        write_database_zinc(&dir, &[(1100, "a", false, true), (1200, "b", false, true)]);
        reload_config(&mut engine, dir.path()).unwrap();
        assert!(engine.polls.contains(1100));
        assert!(engine.polls.contains(1200));

        // Reload: only 1100 has cur marker, 1200 doesn't
        // This means 1200 should be removed from polls
        {
            let content = "ver:\"3.0\"\nchannel,enabled,navName,cur\n\
                           1100,M,\"a\",M\n\
                           1200,M,\"b\",\n";
            fs::write(dir.path().join("database.zinc"), content).unwrap();
        }

        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert!(engine.polls.contains(1100), "1100 should still be polled");
        assert!(
            !engine.polls.contains(1200),
            "1200 should no longer be polled"
        );
        assert_eq!(result.polls_removed, 1, "one poll should have been removed");
    }

    // ── S-56: a table set with holes is not a reload (R-63) ──

    const CSV_HEADER: &str =
        "name,description,path,unit_type,f_min,f_max,c_min,c_max,k_min,k_max,tag";

    /// tables.csv with one table per tag, in the given order; the data file
    /// of a table is `<tag>.txt`.
    fn write_tables(dir: &std::path::Path, tags: &[&str]) {
        let mut body = vec![CSV_HEADER.to_string()];
        for (i, tag) in tags.iter().enumerate() {
            body.push(format!(
                "{tag},Test,{tag}.txt,temp,-40,303,-40,150,233,423,{tag}"
            ));
            let base = (i as u32 + 1) * 1000;
            fs::write(
                dir.join(format!("{tag}.txt")),
                format!("{}\n{}\n{}\n", base, base + 100, base + 200),
            )
            .unwrap();
        }
        fs::write(dir.join("tables.csv"), body.join("\n")).unwrap();
    }

    /// An engine whose tables are loaded from `tags` and whose channel
    /// 1100 is bound to the table `bound`.
    fn engine_bound_to(tags: &[&str], bound: &str) -> Engine<MockHal> {
        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), tags);
        let mut engine = make_engine_with_channel(1100);
        loader::load_tables(
            &mut engine.tables,
            &dir.path().join("tables.csv"),
            dir.path(),
        )
        .unwrap();
        let index = engine.tables.find_by_tag(bound).unwrap();
        engine.channels.get_mut(1100).unwrap().conv.table_index = Some(index);
        engine
    }

    fn bound_tag(engine: &Engine<MockHal>) -> Option<String> {
        let index = engine.channels.get(1100).unwrap().conv.table_index?;
        engine.tables.get(index).map(|t| t.tag.clone())
    }

    #[test]
    fn a_reload_with_a_skipped_table_is_an_error_and_changes_nothing() {
        let mut engine = engine_bound_to(&["a", "b"], "b");
        engine.polls.add(1100).unwrap();

        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["a", "b", "c"]);
        fs::write(dir.path().join("b.txt"), "1\nx\n3\n").unwrap();
        fs::remove_file(dir.path().join("c.txt")).unwrap();
        // A database that would fail too, if the reload got that far.
        fs::write(dir.path().join("database.zinc"), "not zinc {{{\n").unwrap();

        let err = reload_config(&mut engine, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 table(s) not loaded"), "{err}");
        assert!(
            err.contains("b (line 3)") && err.contains("c (line 4)"),
            "{err}"
        );
        assert!(!err.contains("database.zinc"), "{err}");

        assert_eq!(engine.tables.count(), 2);
        assert_eq!(bound_tag(&engine).as_deref(), Some("b"));
        let b = engine
            .tables
            .get(engine.tables.find_by_tag("b").unwrap())
            .unwrap();
        assert_eq!(b.values, vec![2000.0, 2100.0, 2200.0]);
        assert_eq!(engine.polls.count(), 1);
    }

    #[test]
    fn a_reload_of_a_directory_with_neither_file_is_an_error() {
        let dir = TempDir::new().unwrap();
        let mut engine = engine_bound_to(&["a"], "a");
        engine.channels.get_mut(1100).unwrap().enabled = true;

        let err = reload_config(&mut engine, dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to reload"), "{err}");
        assert!(!err.to_lowercase().contains("not found"), "{err}");
        assert_eq!(engine.tables.count(), 1);
        assert_eq!(engine.polls.count(), 0, "the poll list was rebuilt");
    }

    /// The channel holds an index; the new file lists the tables in another
    /// order. Without a database file nothing else would fix the index.
    #[test]
    fn a_reload_that_reorders_the_tables_keeps_the_channel_on_its_table() {
        let mut engine = engine_bound_to(&["a", "b", "c"], "c");
        assert_eq!(engine.channels.get(1100).unwrap().conv.table_index, Some(2));

        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["c", "a", "b"]);
        reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(engine.channels.get(1100).unwrap().conv.table_index, Some(0));
        assert_eq!(bound_tag(&engine).as_deref(), Some("c"));
    }

    #[test]
    fn a_reload_without_the_table_of_a_channel_marks_the_channel() {
        let mut engine = engine_bound_to(&["a", "b"], "b");

        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["a", "z"]);
        reload_config(&mut engine, dir.path()).unwrap();

        // Not index 1 (now "z"), and not "no table".
        assert_eq!(
            engine.channels.get(1100).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );
        assert_eq!(bound_tag(&engine), None);

        // It stays that way over a later reload that still lacks the table.
        reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(
            engine.channels.get(1100).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );
    }

    /// The tables go back after a bad database; so must the indices that
    /// were moved to fit the new tables.
    #[test]
    fn a_bad_database_puts_the_table_indices_back_too() {
        let mut engine = engine_bound_to(&["a", "b", "c"], "c");

        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["c", "a"]);
        fs::write(dir.path().join("database.zinc"), "not zinc {{{\n").unwrap();

        reload_config(&mut engine, dir.path()).unwrap_err();

        assert_eq!(engine.tables.count(), 3);
        assert_eq!(engine.channels.get(1100).unwrap().conv.table_index, Some(2));
        assert_eq!(bound_tag(&engine).as_deref(), Some("c"));
    }

    #[test]
    fn a_reload_with_a_tables_csv_that_lists_no_table_is_an_error() {
        for body in ["", "name,description,path\n", "\n\n"] {
            let mut engine = engine_bound_to(&["a", "b"], "b");
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("tables.csv"), body).unwrap();

            let err = reload_config(&mut engine, dir.path())
                .unwrap_err()
                .to_string();
            assert!(err.contains("lists no table"), "{body:?}: {err}");
            assert_eq!(engine.tables.count(), 2);
            assert_eq!(bound_tag(&engine).as_deref(), Some("b"));
        }
    }

    /// database.zinc with one physical input row for channel 1100 that
    /// names the table `marker`.
    fn write_database_with_table_marker(dir: &std::path::Path, marker: &str) {
        fs::write(
            dir.join("database.zinc"),
            format!(
                "ver:\"3.0\"\nchannel,enabled,navName,unit,{marker}\n1100,M,\"probe\",\"°C\",M\n"
            ),
        )
        .unwrap();
    }

    /// The usual case: there is a database.zinc, its row names a table and
    /// the new tables.csv no longer has it. The database step rebuilds the
    /// channel's conversion; it must not come out without a table.
    #[test]
    fn a_reload_with_a_database_row_that_names_a_missing_table_marks_the_channel() {
        let mut engine = engine_bound_to(&["thermistor10K2", "pt1000"], "thermistor10K2");

        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["pt1000"]);
        write_database_with_table_marker(dir.path(), "thermistor10K2");
        reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(
            engine.channels.get(1100).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );

        // With the table back, the next reload binds it again.
        write_tables(dir.path(), &["pt1000", "thermistor10K2"]);
        reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(bound_tag(&engine).as_deref(), Some("thermistor10K2"));
    }

    /// The marked channel reads as fault; no table is picked for it by its
    /// channel number.
    #[test]
    fn a_channel_marked_as_missing_its_table_reads_as_fault() {
        let mut engine = engine_bound_to(&["thermistor10K2", "pt1000"], "pt1000");
        engine.channels.get_mut(1100).unwrap().enabled = true;

        let dir = TempDir::new().unwrap();
        // 1100 would auto-detect range0to10V; give it one to pick.
        write_tables(dir.path(), &["thermistor10K2", "range0to10V"]);
        write_database_with_table_marker(dir.path(), "pt1000");
        reload_config(&mut engine, dir.path()).unwrap();

        // device 0, address 0: the channel of make_engine_with_channel.
        engine.hal.set_analog(0, 0, Ok(2048.0));
        let value = engine.channel_read(1100).unwrap();
        assert_eq!(value.status, sandstar_engine::EngineStatus::Fault);
        assert_eq!(
            engine.channels.get(1100).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );
    }

    // ── S-59: an auto-detected sensor is not "modified" on every reload ──

    /// database.zinc with one polled physical row for channel 1113 that
    /// names thermistor10K2 and gives no low/high (the device's row).
    fn write_sensor_row(dir: &std::path::Path, label: &str) {
        fs::write(
            dir.join("database.zinc"),
            format!(
                "ver:\"3.0\"\nchannel,enabled,navName,unit,cur,thermistor10K2\n1113,M,\"{label}\",\"°C\",M,M\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn an_auto_detected_sensor_is_unchanged_on_a_reload_of_the_same_config() {
        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["thermistor10K2"]);
        write_sensor_row(dir.path(), "probe");
        let mut engine = make_engine_with_channel(1113);
        reload_config(&mut engine, dir.path()).unwrap();

        // The first read completes the conversion from the table.
        engine.hal.set_analog(0, 0, Ok(1100.0));
        let before = engine.channel_read(1113).unwrap();
        let conv = engine.channels.get(1113).unwrap().conv.clone();
        assert!(
            conv.low.is_some() && conv.high.is_some(),
            "not auto-detected"
        );

        let summary = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(summary.channels_updated, 0, "{summary}");
        assert_eq!(summary.channels_unchanged, 1, "{summary}");

        // The reading is the same after the reload.
        let after = engine.channel_read(1113).unwrap();
        assert_eq!(after.status, before.status);
        assert_eq!(after.cur, before.cur);

        // Before any read (a reload right after a reload) it is unchanged too.
        let summary = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(summary.channels_updated, 0, "{summary}");
        let summary = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(summary.channels_updated, 0, "{summary}");
    }

    #[test]
    fn a_real_change_to_an_auto_detected_sensor_still_counts_as_modified() {
        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["thermistor10K2"]);
        write_sensor_row(dir.path(), "probe");
        let mut engine = make_engine_with_channel(1113);
        reload_config(&mut engine, dir.path()).unwrap();
        engine.hal.set_analog(0, 0, Ok(1100.0));
        engine.channel_read(1113).unwrap();

        write_sensor_row(dir.path(), "renamed");
        let summary = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(summary.channels_updated, 1, "{summary}");
        assert_eq!(engine.channels.get(1113).unwrap().label, "renamed");
    }

    // ── S-60: tables by channel number, and the two tags without a number ──

    /// database.zinc with one polled physical row: a unit, and optionally a
    /// marker column.
    fn write_row(dir: &std::path::Path, channel: u32, marker: Option<&str>) {
        let (col, cell) = match marker {
            Some(m) => (format!(",{m}"), ",M"),
            None => (String::new(), ""),
        };
        fs::write(
            dir.join("database.zinc"),
            format!(
                "ver:\"3.0\"\nchannel,enabled,navName,unit,cur{col}\n{channel},M,\"probe\",\"°C\",M{cell}\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_sensor_faults_when_its_table_leaves_tables_csv_and_recovers_when_it_returns() {
        let dir = TempDir::new().unwrap();
        // 1122 stands for pt1000; the row names no table.
        write_tables(dir.path(), &["pt1000", "thermistor10K2"]);
        write_row(dir.path(), 1122, None);
        let mut engine = make_engine_with_channel(1122);
        reload_config(&mut engine, dir.path()).unwrap();
        engine.hal.set_analog(0, 0, Ok(1100.0));
        assert_eq!(bound_tag_of(&engine, 1122).as_deref(), None);
        let good = engine.channel_read(1122).unwrap();
        assert_eq!(good.status, sandstar_engine::EngineStatus::Ok);
        assert_eq!(bound_tag_of(&engine, 1122).as_deref(), Some("pt1000"));

        // pt1000 is gone, other tables remain.
        write_tables(dir.path(), &["thermistor10K2"]);
        reload_config(&mut engine, dir.path()).unwrap();
        let value = engine.channel_read(1122).unwrap();
        assert_eq!(value.status, sandstar_engine::EngineStatus::Fault);
        assert_eq!(
            engine.channels.get(1122).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );
        // A reload of the same files does not count the channel as modified.
        let summary = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(summary.channels_updated, 0, "{summary}");

        // The table is back (second in the file now: values 2000..2200).
        write_tables(dir.path(), &["thermistor10K2", "pt1000"]);
        reload_config(&mut engine, dir.path()).unwrap();
        engine.hal.set_analog(0, 0, Ok(2100.0));
        let value = engine.channel_read(1122).unwrap();
        assert_eq!(value.status, sandstar_engine::EngineStatus::Ok);
        assert_eq!(bound_tag_of(&engine, 1122).as_deref(), Some("pt1000"));
    }

    fn bound_tag_of(engine: &Engine<MockHal>, channel: u32) -> Option<String> {
        let index = engine.channels.get(channel).unwrap().conv.table_index?;
        engine.tables.get(index).map(|t| t.tag.clone())
    }

    #[test]
    fn a_row_can_name_the_two_tables_that_have_no_channel_number() {
        for tag in ["thermistor20K4", "thermistor10K2_wts"] {
            let dir = TempDir::new().unwrap();
            write_tables(dir.path(), &["pt1000", tag]);
            // 500 is outside the auto-detected range.
            write_row(dir.path(), 500, Some(tag));
            let mut engine = make_engine_with_channel(500);
            reload_config(&mut engine, dir.path()).unwrap();
            assert_eq!(bound_tag_of(&engine, 500).as_deref(), Some(tag));

            // Named but not loaded: marked, as for the other tags.
            write_tables(dir.path(), &["pt1000"]);
            reload_config(&mut engine, dir.path()).unwrap();
            assert_eq!(
                engine.channels.get(500).unwrap().conv.table_index,
                Some(MISSING_TABLE),
                "{tag}"
            );
        }
    }

    /// No database.zinc at all: nothing rebuilds the channel's conversion,
    /// so the reload itself has to take the mark off when the table returns.
    #[test]
    fn a_marked_sensor_without_a_database_row_recovers_when_its_table_returns() {
        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["thermistor10K2"]);
        let mut engine = make_engine_with_channel(1122);
        engine.channels.get_mut(1122).unwrap().conv.unit = "°C".into();
        reload_config(&mut engine, dir.path()).unwrap();
        engine.hal.set_analog(0, 0, Ok(2100.0));
        assert_eq!(
            engine.channel_read(1122).unwrap().status,
            sandstar_engine::EngineStatus::Fault
        );

        // Still no pt1000: the mark stays.
        reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(
            engine.channels.get(1122).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );

        write_tables(dir.path(), &["thermistor10K2", "pt1000"]);
        reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(engine.channels.get(1122).unwrap().conv.table_index, None);
        let value = engine.channel_read(1122).unwrap();
        assert_eq!(value.status, sandstar_engine::EngineStatus::Ok);
        assert_eq!(bound_tag_of(&engine, 1122).as_deref(), Some("pt1000"));
    }

    /// A failed reload puts the mark back with the tables.
    #[test]
    fn a_bad_database_puts_an_auto_detection_mark_back() {
        let dir = TempDir::new().unwrap();
        write_tables(dir.path(), &["thermistor10K2"]);
        let mut engine = make_engine_with_channel(1122);
        engine.channels.get_mut(1122).unwrap().conv.unit = "°C".into();
        reload_config(&mut engine, dir.path()).unwrap();
        engine.hal.set_analog(0, 0, Ok(2100.0));
        engine.channel_read(1122).unwrap();

        write_tables(dir.path(), &["thermistor10K2", "pt1000"]);
        fs::write(dir.path().join("database.zinc"), "not zinc {{{\n").unwrap();
        reload_config(&mut engine, dir.path()).unwrap_err();

        assert_eq!(engine.tables.count(), 1);
        assert_eq!(
            engine.channels.get(1122).unwrap().conv.table_index,
            Some(MISSING_TABLE)
        );
    }
}
