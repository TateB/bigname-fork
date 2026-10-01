//! The full-state loader's session across a completed redo, over the Lifecycle history.
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use super::*;

const RESTORE_STARTED: &str = "interpret is restoring prior adapter state from stored events";
const REORG_REASON: &str =
    "a chain reorganization since the last batch invalidated the retained session";

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    fn lines_with(&self, needle: &str) -> Vec<String> {
        String::from_utf8(self.0.lock().expect("log lock").clone())
            .expect("UTF-8 logs")
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}

impl Write for Logs {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log lock").extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn full_state_engine(pool: &PgPool) -> Engine {
    Engine::new(pool.clone())
        .with_blocks_per_batch(NonZeroU32::new(3).expect("positive batch"))
        .with_full_state_loader_forced(true)
}

async fn walk_normal(engine: &Engine, mut current: Option<Marker>, to: i64) -> TestResult<Marker> {
    loop {
        let marker = run_batch(engine, current, to).await?;
        if marker.number >= to {
            return Ok(marker);
        }
        current = Some(marker);
    }
}

/// Bumps the chain's lineage orphaning epoch, as Live does when it orphans a block.
async fn record_orphaning(pool: &PgPool, head: i64) -> TestResult {
    sqlx::query(
        "INSERT INTO chain_heads (
             chain_id, latest_block_hash, latest_block_number, lineage_orphaning_epoch
         ) VALUES ($1, $2, $3, 1)",
    )
    .bind(CHAIN)
    .bind(block_hash(head))
    .bind(head)
    .execute(pool)
    .await?;
    Ok(())
}

/// A redo that ends where Normal stands leaves its session for Normal's next batch, so after
/// the redo's own restore Normal continues without restoring history again, unless the
/// chain's lineage orphaning epoch moved in between. Either way the stored events equal a
/// walk with no redo.
#[tokio::test]
async fn normal_resumes_from_the_session_a_completed_redo_leaves() -> TestResult {
    let last_block = History::Lifecycle.last_block();
    let (redo_from, redo_to) = (FIRST_BLOCK + 3, FIRST_BLOCK + 7);
    let expected = {
        let database = database("interpret_redo_session_expected").await?;
        seed_history(database.pool(), History::Lifecycle).await?;
        walk_normal(&full_state_engine(database.pool()), None, last_block).await?;
        let stored = stored_events(database.pool()).await?;
        database.cleanup().await?;
        stored
    };

    for orphaned_after_redo in [false, true] {
        let database = database("interpret_redo_session_handoff").await?;
        let pool = database.pool();
        seed_history(pool, History::Lifecycle).await?;
        let engine = full_state_engine(pool);
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer({
                let logs = logs.clone();
                move || logs.clone()
            })
            .finish();
        let _logs = tracing::subscriber::set_default(subscriber);

        let normal = walk_normal(&engine, None, redo_to).await?;
        let mut redo = None;
        loop {
            let outcome = engine
                .run_batch(BatchRequest {
                    chain_id: CHAIN.to_owned(),
                    from_block: redo_from,
                    to_block: redo_to,
                    resume_current: redo,
                    mode: RunMode::Redo,
                })
                .await?;
            redo = Some(outcome.current);
            if outcome.complete {
                break;
            }
        }
        assert_eq!(redo.as_ref(), Some(&normal));
        assert_eq!(
            logs.lines_with(RESTORE_STARTED).len(),
            2,
            "Normal's first batch and the redo's first batch each restore history"
        );
        if orphaned_after_redo {
            record_orphaning(pool, last_block).await?;
        }

        walk_normal(&engine, Some(normal), last_block).await?;
        let restores = logs.lines_with(RESTORE_STARTED);
        if orphaned_after_redo {
            assert_eq!(restores.len(), 3, "{restores:#?}");
            assert!(restores[2].contains(REORG_REASON), "{restores:#?}");
        } else {
            assert_eq!(
                restores.len(),
                2,
                "Normal resumed from the redo's session: {restores:#?}"
            );
        }
        assert_eq!(
            logs.lines_with("interpret restored prior adapter state")
                .len(),
            restores.len()
        );
        assert_eq!(
            stored_events(pool).await?,
            expected,
            "orphaned after redo: {orphaned_after_redo}"
        );
        database.cleanup().await?;
    }
    Ok(())
}
