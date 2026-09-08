use std::path::Path;

use std::sync::Arc;

use crate::events::store::{open_store, EventStore, StoreBackend};

use super::{runtime, store_path};

pub fn exec(stream: Option<&str>, root: impl AsRef<Path>) -> Result<(), String> {
    let db_path = store_path(root.as_ref());
    if !db_path.exists() {
        return Err(format!(
            "no event store found at {}. Run a pipeline first or check --dir.",
            db_path.display()
        ));
    }

    let store = open_store(StoreBackend::Sqlite { path: db_path.clone() })
        .map_err(|e| format!("failed to open event store: {e}"))?;

    let rt = runtime();

    match stream {
        Some(stream_id) => verify_stream(&store, &rt, stream_id),
        None => {
            let mut streams = rt
                .block_on(store.list_streams())
                .map_err(|e| format!("failed to list streams: {e}"))?;

            // Union in streams that have a recorded head but no rows: a
            // whole-stream deletion leaves the head behind (ADR-0040), and we
            // still want verify to flag it. Such a stream verifies with 0 rows
            // against a non-empty head → truncation error.
            let known: std::collections::HashSet<&str> =
                streams.iter().map(|(s, _)| s.as_str()).collect();
            let head_ids = rt
                .block_on(store.head_stream_ids())
                .map_err(|e| format!("failed to list stream heads: {e}"))?;
            let missing: Vec<String> = head_ids
                .into_iter()
                .filter(|s| !known.contains(s.as_str()))
                .collect();
            for s in missing {
                streams.push((s, 0));
            }
            streams.sort();

            if streams.is_empty() {
                println!("{}", summary(0, 0, 0));
                return Ok(());
            }

            let mut all_ok = true;
            let mut verified_total = 0u64;
            let mut legacy_total = 0u64;
            for (stream_id, event_count) in &streams {
                match rt.block_on(store.verify_chain(stream_id)) {
                    Ok(v) if v.legacy > 0 => {
                        println!(
                            "  {stream_id}: {}/{event_count} events OK ({} legacy, pre-hash — exempt)",
                            v.verified, v.legacy
                        );
                        verified_total += v.verified;
                        legacy_total += v.legacy;
                    }
                    Ok(v) => {
                        println!("  {stream_id}: {}/{event_count} events OK", v.verified);
                        verified_total += v.verified;
                    }
                    Err(e) => {
                        println!("  {stream_id}: FAILED — {e}");
                        all_ok = false;
                    }
                }
            }

            println!();
            if all_ok {
                println!("{}", summary(streams.len(), verified_total, legacy_total));
            } else {
                return Err("hash chain verification failed for one or more streams".into());
            }

            Ok(())
        }
    }
}

/// Human summary for a whole-store verification that did not fail.
///
/// Always carries its denominator. A run that checked nothing must not be
/// shaped like a run that checked everything — external finding F5, reported
/// by nochnoy-provodecz against 0.9.0, where an empty store printed
/// "Nothing to verify." and exited 0, indistinguishable from a completed pass.
fn summary(streams: usize, verified: u64, legacy: u64) -> String {
    let mut s = format!("Verified {verified} event(s) across {streams} stream(s)");
    if legacy > 0 {
        s.push_str(&format!(" ({legacy} legacy event(s) pre-hash — exempt)"));
    }
    s.push('.');
    if verified == 0 {
        s.push_str(
            "\nNothing was checked: this is a vacuous pass, not a completed verification.",
        );
    }
    s
}

fn verify_stream(
    store: &Arc<dyn EventStore>,
    rt: &tokio::runtime::Runtime,
    stream_id: &str,
) -> Result<(), String> {
    match rt.block_on(store.verify_chain(stream_id)) {
        Ok(v) => {
            if v.total() == 0 {
                println!(
                    "Stream '{stream_id}': verified 0 of 0 event(s). \
                     Nothing was checked: this is a vacuous pass, not a completed verification."
                );
            } else if v.verified == 0 {
                println!(
                    "Stream '{stream_id}': verified 0 of {} event(s) — all {} predate the hash \
                     chain and are exempt. Nothing was checked: this is a vacuous pass, not a \
                     completed verification.",
                    v.total(),
                    v.legacy
                );
            } else if v.legacy > 0 {
                println!(
                    "Stream '{stream_id}': {} event(s) verified, hash chain intact \
                     ({} legacy event(s) exempt).",
                    v.verified, v.legacy
                );
            } else {
                println!(
                    "Stream '{stream_id}': {} event(s) verified, hash chain intact.",
                    v.verified
                );
            }
            Ok(())
        }
        Err(e) => Err(format!("stream '{stream_id}': hash chain BROKEN — {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::summary;

    #[test]
    fn empty_store_states_its_denominator_and_labels_the_vacuum() {
        let s = summary(0, 0, 0);
        assert!(s.contains("0 event(s) across 0 stream(s)"), "{s}");
        assert!(s.contains("vacuous pass"), "{s}");
    }

    #[test]
    fn completed_pass_is_not_labelled_vacuous() {
        let s = summary(9, 1204, 0);
        assert!(s.contains("1204 event(s) across 9 stream(s)"), "{s}");
        assert!(!s.contains("vacuous"), "{s}");
    }

    #[test]
    fn streams_that_exist_but_hold_only_exempt_events_are_still_vacuous() {
        // Nine streams, none of them hash-chained: the run is shaped like a
        // pass and checked nothing. This is the case a stream count alone
        // would hide, which is why the event count leads.
        let s = summary(9, 0, 41);
        assert!(s.contains("0 event(s) across 9 stream(s)"), "{s}");
        assert!(s.contains("41 legacy"), "{s}");
        assert!(s.contains("vacuous pass"), "{s}");
    }

    #[test]
    fn legacy_events_are_reported_alongside_a_real_pass() {
        let s = summary(2, 5, 3);
        assert!(s.contains("5 event(s) across 2 stream(s)"), "{s}");
        assert!(s.contains("3 legacy"), "{s}");
        assert!(!s.contains("vacuous"), "{s}");
    }
}
