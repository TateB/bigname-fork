use super::V1NodeRequest;
use std::{cell::RefCell, collections::BTreeSet};

// The adapter restore and prepare calls are synchronous. A restore or batch that reads a name
// outside the loaded set fails with `UnloadedNames`, and its output is discarded. The loader
// may load those names as well and try again: the collector cannot know every name in
// advance, because ENSv2 derives a token's name from registry state during the batch. Only
// an attempt that read no unloaded name is used, so the certificate below is the same however
// many attempts it took.
//
// A read of per-name ENSv1-model state (the ENSv1 families and the Basenames Base families,
// which share that state) is reported here when its key is built by `v1_key` or
// `v1_surface_key` (state_registrar.rs). Reads that build or receive their key another way,
// and why each cannot reach a name that was not loaded:
//
// - `settle_v1_releases` (state_expiry.rs) takes keys from the expiry index. It reports
//   every due key itself before releasing it.
// - `known_surfaces` and `active_resources` reads and writes keyed by a stored
//   `logical_name_id` (`promote_known_v1_authority`, `observe_v1_active_surface`,
//   `materialize_v1_active_surface`, `bind_v1_active_surface` in state_surfaces.rs;
//   `observe_v1_name`, `observe_v1_registrar`, `activate_v1_authority`,
//   `reactivate_v1_registrar*` in state.rs; `release_v1_name` in state_expiry.rs): each
//   sits in a function that calls `v1_key` for the same name first, or is handed the key
//   its caller built with `v1_key`, so the name is already reported.
// - `v1_registry_authority_if_authentic` (state.rs) and the binding helpers in
//   state_surfaces.rs take a prebuilt key: callers pass a `v1_key` result, except
//   `settle_v1_releases`, covered above.
// - `v1_registrar_controllers`, `v1_pending_wrapper_sync_expiries` and the renewal
//   expiries of `v1_registrar_transaction` (state_wrapper.rs) are iterated and cleared
//   whole, but they are emptied at the start of every transaction, so they never hold
//   state from before the batch.
// - `surface_removal_candidates` (state_incremental.rs) is iterated whole; it holds only
//   names the current batch or restore touched through the functions above. The wholesale
//   map replacement in the same file moves a session's state, it reads no name.
// - ENSv2 state (`v2_*` in state.rs) is restored from every retained ENSv2 event, so it is
//   complete and its reads need no report. Where ENSv2 code reads the name-keyed state it
//   shares with ENSv1-model names (`known_surfaces`, `active_resources` and the restored
//   surface counts: `name_link_by_namehash`, `v2_resolver_hint`, `remove_v2_active_resource`
//   in state_v2.rs, the active-resource check in state_v2_refresh.rs, and
//   `prune_unbacked_surfaces` in state_incremental.rs), it reports the name with
//   `observe_name`. Its writes there insert or overwrite a value regardless of the prior one.
thread_local! {
    static COVERAGE: RefCell<Option<Coverage>> = const { RefCell::new(None) };
}
struct Coverage {
    loaded: BTreeSet<String>,
    missing: BTreeSet<String>,
}
struct Clear;
impl Drop for Clear {
    fn drop(&mut self) {
        COVERAGE.with_borrow_mut(|scope| *scope = None);
    }
}

pub(super) fn checked<T>(
    nodes: &BTreeSet<V1NodeRequest>,
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    COVERAGE.with_borrow_mut(|scope| {
        anyhow::ensure!(scope.is_none(), "nested V1 lookahead preparation");
        *scope = Some(Coverage {
            loaded: nodes
                .iter()
                .map(|n| format!("{}:{}", n.namespace, n.node))
                .collect(),
            missing: BTreeSet::new(),
        });
        Ok::<_, anyhow::Error>(())
    })?;
    let _clear = Clear;
    let result = operation();
    let missing =
        COVERAGE.with_borrow_mut(|scope| std::mem::take(&mut scope.as_mut().unwrap().missing));
    if missing.is_empty() {
        return result;
    }
    let names = missing
        .into_iter()
        .map(|key| {
            let (namespace, node) = key
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("lookahead name key {key} has no namespace"))?;
            // The loader requests names in this spelling, so a key in any other could never
            // load and would pass the check on the next attempt.
            let node: alloy_primitives::B256 = node
                .parse()
                .map_err(|_| anyhow::anyhow!("lookahead name key {key} is not a namehash"))?;
            Ok(V1NodeRequest {
                namespace: namespace.to_owned(),
                node: format!("{node:#x}"),
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Err(UnloadedNames(names).into())
}

/// The names a lookahead restore or batch read without their history being loaded. The
/// attempt's output is discarded; the caller may load these names too and try again.
#[derive(Debug)]
pub struct UnloadedNames(pub BTreeSet<V1NodeRequest>);

impl std::fmt::Display for UnloadedNames {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "lookahead accessed unloaded names: {:?}", self.0)
    }
}

impl std::error::Error for UnloadedNames {}

/// Report a read of name-keyed state shared between the ENSv1-model and ENSv2 code
/// (`known_surfaces`, `active_resources` and the restored surface counts) by its logical name.
pub(in crate::schema_v2) fn observe_name(logical_name_id: &str) {
    observe_node(&logical_name_id.to_ascii_lowercase());
}

pub(in crate::schema_v2) fn observe_node(key: &str) {
    COVERAGE.with_borrow_mut(|scope| {
        if let Some(scope) = scope
            && !scope.loaded.contains(key)
        {
            scope.missing.insert(key.to_owned());
        }
    });
}
