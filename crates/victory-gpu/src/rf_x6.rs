//! Release diagnostics for passes that did not reach the GPU.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::object::This;

pub(crate) fn stats() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RF_X6_STATS").is_some_and(|v| v == "1"))
}

/// Opt back into the identified effects. Heat haze requires the early depth
/// fix so that transparent surfaces behind characters cannot write depth.
pub(super) fn restore_left_out(hash: &str) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RF_X6_RESTORE_LEFTOUT").is_some_and(|v| v == "1"))
        && match hash {
            "02384a60fda6f2d0" => true,
            "731a39171b568298" => victory_shader::emit::early_depth(),
            _ => false,
        }
}

/// Names in the console catalog carry a variant suffix, e.g.
/// CS_ShadowRenderTiledCascadesCB_9945271c_Wrapped.
pub(super) fn entry_name(bytes: &[u8]) -> Option<&str> {
    for (i, _) in bytes.windows(8).enumerate().filter(|(_, b)| *b == b"_Wrapped") {
        let start = bytes[..i].iter().rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_')).map_or(0, |at| at + 1);
        let candidate = std::str::from_utf8(&bytes[start..i + 8]).ok()?;
        if ["CS_", "PS_", "VS_", "MS_", "AS_", "GS_", "HS_", "DS_"].into_iter().any(|prefix| candidate.starts_with(prefix)) {
            return Some(candidate);
        }
    }
    None
}

#[derive(Default)]
struct Count {
    shaders: Vec<(String, String)>,
    calls: u64,
}

type Reasons = HashMap<(&'static str, &'static str), HashMap<String, Count>>;
static COUNTS: Mutex<Option<Reasons>> = Mutex::new(None);

/// Count actual command rejections, since host pipeline lookup also occurs
/// several times during a successful draw.
pub(crate) fn record(this: This, command: &'static str, reason: Option<&'static str>) {
    if !stats() {
        return;
    }
    let pipeline = (!this.is_null()).then(|| super::pipeline(this));
    let reason = reason.unwrap_or_else(|| pipeline.map_or("no title pipeline bound", |p| p.host.drop_reason()));
    let key = pipeline.map_or("-", |p| p.host.key());
    let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let entries = counts.get_or_insert_with(HashMap::new).entry((command, reason)).or_default();
    if let Some(count) = entries.get_mut(key) {
        count.calls += 1;
    } else {
        let shaders = pipeline.map_or_else(Vec::new, |p| {
            p.shaders.iter().map(|s| (s.hash.clone(), s.diagnostic_name())).collect()
        });
        entries.insert(key.to_owned(), Count { shaders, calls: 1 });
    }
}

pub(crate) fn shadow(this: This, status: &'static str) {
    if stats() && !this.is_null()
        && super::pipeline(this).shaders.iter().any(|s| s.hash == "be6ce5d849d51127")
    {
        record(this, "indirect cascaded shadow", Some(status));
    }
}

pub(crate) fn line(frames: f64) {
    if !stats() {
        return;
    }
    let counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    for ((command, reason), psos) in counts {
        for (pso, count) in psos {
            tracing::info!(
                command, reason, %pso, shaders = ?count.shaders,
                calls = count.calls, per_frame = count.calls as f64 / frames.max(1.0),
                "rf-x6: pipeline activity"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::entry_name;

    #[test]
    fn reads_wrapped_shader_entry_from_binary_container() {
        assert_eq!(
            entry_name(b"\0text_Wrapped\0CS_ShadowRenderTiledCascadesCB_9945271c_Wrapped\0"),
            Some("CS_ShadowRenderTiledCascadesCB_9945271c_Wrapped")
        );
        assert_eq!(entry_name(b"\0text_Wrapped\0"), None);
    }
}
