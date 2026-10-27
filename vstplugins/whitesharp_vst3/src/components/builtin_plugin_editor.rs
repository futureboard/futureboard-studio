//! Studio's state mirror, as far as the WhiteSharp editor reads it: the
//! params of the instance an editor is bound to, here the plug-in's own.

use std::sync::{Arc, Mutex, Weak};

use crate::params::WhiteSharpParams;

/// Open editors' instances by insert id. Touched only when an editor opens
/// and when it re-reads its params, never by the audio thread.
static INSTANCES: Mutex<Vec<(String, Weak<WhiteSharpParams>)>> = Mutex::new(Vec::new());

pub fn register(insert_id: &str, params: &Arc<WhiteSharpParams>) {
    let mut instances = INSTANCES.lock().unwrap_or_else(|e| e.into_inner());
    instances.retain(|(id, params)| id != insert_id && params.strong_count() > 0);
    instances.push((insert_id.to_string(), Arc::downgrade(params)));
}

pub fn builtin_whitesharp_params(insert_id: &str) -> Option<whitesharp::Params> {
    let instances = INSTANCES.lock().unwrap_or_else(|e| e.into_inner());
    let (_, params) = instances.iter().find(|(id, _)| id == insert_id)?;
    Some(params.upgrade()?.to_dsp_params())
}
