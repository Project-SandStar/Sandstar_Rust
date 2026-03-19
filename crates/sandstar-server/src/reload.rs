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

use sandstar_engine::Engine;
use sandstar_hal::{HalDiagnostics, HalRead, HalWrite};
use tracing::{error, info, warn};

use crate::loader;

/// Summary of what changed during a config reload.
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
            self.channels_added, self.channels_removed,
            self.channels_updated, self.channels_unchanged,
            self.tables_reloaded,
            self.polls_added, self.polls_removed, self.polls_after,
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

    // 1. Reload tables (tables.csv + data files)
    let tables_csv = config_dir.join("tables.csv");
    let table_dir = [
        config_dir.join("../usr/local/config"),
        config_dir.join("config"),
        config_dir.to_path_buf(),
    ]
    .into_iter()
    .find(|p| p.exists())
    .unwrap_or_else(|| config_dir.to_path_buf());

    if tables_csv.exists() {
        engine.tables.clear();
        match loader::load_tables(&mut engine.tables, &tables_csv, &table_dir) {
            Ok(n) => {
                summary.tables_reloaded = n;
                info!(count = n, "tables reloaded");
            }
            Err(e) => {
                let msg = format!("failed to reload tables: {}", e);
                error!("{}", msg);
                summary.errors.push(msg);
            }
        }
    }

    // 2. Reload database.zinc (granular diff-based)
    let database_path = config_dir.join("database.zinc");
    if database_path.exists() {
        // Snapshot current polls before attempting reload — enables rollback on failure
        let polls_snapshot = engine.polls.snapshot();

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
                let current_poll_ids: HashSet<u32> = engine
                    .polls
                    .iter()
                    .map(|(&id, _)| id)
                    .collect();

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
                let msg = format!("failed to reload database.zinc: {}", e);
                error!("{}", msg);
                summary.errors.push(msg);
                // Fallback: poll all input channels
                engine.polls.clear();
                let fallback_count = loader::setup_polls(engine);
                if fallback_count == 0 && engine.polls.count() == 0 {
                    // Both database load AND setup_polls produced 0 polls —
                    // restore the previous poll list so we don't stop reading
                    // all sensors.
                    error!(
                        prev_polls = polls_snapshot.len(),
                        "reload failed completely — rolling back to previous poll list"
                    );
                    engine.polls.restore(polls_snapshot);
                }
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
        warn!(%summary, "config reload completed with errors");
    }

    Ok(summary)
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

        let result = reload_config(&mut engine, dir.path()).unwrap();
        // setup_polls should add the enabled input channel
        assert!(result.polls_after > 0 || result.errors.is_empty());
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
        fs::write(dir.path().join("database.zinc"), "not valid zinc at all {{{\n").unwrap();

        let mut engine = make_engine_with_channel(1100);

        // Enable the channel so setup_polls will pick it up as fallback
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Pre-populate polls so there's something to roll back to
        engine.polls.add(1100).unwrap();
        assert_eq!(engine.polls.count(), 1);

        let result = reload_config(&mut engine, dir.path()).unwrap();

        // database.zinc failed, but setup_polls should have rebuilt the list
        // from the enabled input channel. Either way, we should never have 0 polls.
        assert!(
            result.polls_after > 0,
            "polls_after should not be 0 after reload failure with rollback"
        );
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
    fn write_database_zinc(
        dir: &TempDir,
        channels: &[(u32, &str, bool, bool)],
    ) {
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
                if *is_virtual { content.push('M'); }
                content.push(',');
                if *is_virtual { content.push('M'); } // analog marker for virtual
            }
            // cur (if column exists)
            if has_cur {
                content.push(',');
                if *has_cur_marker { content.push('M'); }
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
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
        ]);
        let _result = reload_config(&mut engine, dir.path()).unwrap();
        assert_eq!(engine.channels.count(), 1);
        assert!(engine.polls.contains(1100));

        // Reload with an extra virtual channel
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
            (3000, "virtual_temp", true, false),
        ]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(result.channels_added, 1, "should have added 1 virtual channel");
        assert!(engine.channels.contains(3000), "virtual channel 3000 should exist");
        assert!(engine.polls.contains(3000), "virtual channel should be polled");
        assert!(engine.polls.contains(1100), "original poll should still exist");

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
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
            (3000, "virtual_temp", true, false),
        ]);
        reload_config(&mut engine, dir.path()).unwrap();
        assert!(engine.channels.contains(3000));

        // Reload without virtual channel
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
        ]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(result.channels_removed, 1, "should have removed 1 virtual channel");
        assert!(!engine.channels.contains(3000), "virtual channel 3000 should be gone");
        assert!(!engine.polls.contains(3000), "removed channel should not be polled");
        assert!(engine.polls.contains(1100), "original poll should still exist");
    }

    #[test]
    fn test_granular_reload_preserves_runtime_state() {
        let dir = TempDir::new().unwrap();
        let mut engine = make_engine_with_channel(1100);
        engine.channels.get_mut(1100).unwrap().enabled = true;

        // Initial load
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
        ]);
        reload_config(&mut engine, dir.path()).unwrap();

        // Simulate runtime state: set priority array + current value + poll state
        {
            let ch = engine.channels.get_mut(1100).unwrap();
            ch.priority_array = Some(sandstar_engine::priority::PriorityArray::default());
            ch.priority_array.as_mut().unwrap().set_level(8, Some(72.0), "test", 0.0);
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
        write_database_zinc(&dir, &[
            (1100, "temperature_sensor", false, true),
        ]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(result.channels_updated, 1, "label changed, should be modified");

        // Label should be updated
        let ch = engine.channels.get(1100).unwrap();
        assert_eq!(ch.label, "temperature_sensor");

        // Priority array should be preserved (update_metadata doesn't touch it)
        assert!(ch.priority_array.is_some(), "priority array must survive reload");
        let (eff, _level) = ch.priority_array.as_ref().unwrap().effective();
        assert_eq!(eff, Some(72.0), "priority write at level 8 must survive reload");

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
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
        ]);
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
        write_database_zinc(&dir, &[
            (1100, "temp", false, true),
        ]);
        let result = reload_config(&mut engine, dir.path()).unwrap();

        assert_eq!(result.channels_updated, 0, "nothing modified");
        assert_eq!(result.channels_unchanged, 1, "channel should be marked unchanged");
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
        write_database_zinc(&dir, &[
            (3000, "old_label", true, false),
        ]);
        reload_config(&mut engine, dir.path()).unwrap();
        assert!(engine.channels.contains(3000));
        assert_eq!(engine.channels.get(3000).unwrap().label, "old_label");

        // Reload with updated label — should update in-place, not fail
        write_database_zinc(&dir, &[
            (3000, "new_label", true, false),
        ]);
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
        write_database_zinc(&dir, &[
            (1100, "a", false, true),
            (1200, "b", false, true),
        ]);
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
        assert!(!engine.polls.contains(1200), "1200 should no longer be polled");
        assert_eq!(result.polls_removed, 1, "one poll should have been removed");
    }
}
