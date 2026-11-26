//! The operator Store call router.
use super::{files, snapshot, store};
use crate::StoreService;
use layerfs_bridge::control::{SdkCall, SdkRefusal, SdkValue};

/// Runs one operator Store call once. The read model's history half
/// (`operator.inspect.history`) is the snapshot with no Workspaces, read from
/// the Store alone; `operator.inspect.snapshot` adds a daemon's Workspaces and
/// is the daemon's call.
pub fn dispatch(service: &StoreService, call: &SdkCall) -> Result<SdkValue, SdkRefusal> {
    match call.method.as_str() {
        "operator.inspect.history" => snapshot(&service.store, &call.args, &[]),
        "operator.store.stats" => store::stats(&service.store),
        "operator.store.commits" => store::commits(&service.store, &call.args),
        "operator.store.overwritten" => store::overwritten(&service.store, &call.args),
        "operator.files.stats" => files::stats(&service.store, &call.args),
        "operator.files.history" => files::history(&service.store, &call.args),
        method => Err(SdkRefusal {
            code: "UnknownMethod".into(),
            message: format!("no operator Store method {method}"),
            detail: SdkValue::Null,
        }),
    }
}
