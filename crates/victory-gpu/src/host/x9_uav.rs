//! Observations from comparisons already performed for copy-back. A
//! comparison sees changed bytes, which may be attributable to several
//! shaders in the same submission. These are candidates for investigation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

const PIECE: usize = 64 << 10;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Shader {
    pub hash: u64,
    pub by_address: bool,
    pub indirect: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Root {
    hash: u64,
    slot: u8,
    shadow: u64,
    from: usize,
    to: usize,
}

#[derive(Default)]
pub(super) struct Recorded {
    shaders: BTreeSet<Shader>,
    roots: BTreeSet<Root>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Producer {
    shader: Option<Shader>,
    root: Option<u8>,
    view: Option<(usize, usize)>,
}

type Changed = BTreeMap<(u64, u64, u64, Producer), crate::rf_x9::Ranges>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_submissions_accumulate_changed_pieces_without_growing_per_draw() {
        const SHADOW: u64 = 1;
        let mut recorded = Recorded::default();
        for _ in 0..4000 {
            recorded.root(9, 2, SHADOW, PIECE, PIECE * 4);
            recorded.root(9, 4, SHADOW, PIECE * 3, PIECE * 4);
            recorded.shader(Shader { hash: 9, by_address: false, indirect: true });
        }
        assert_eq!(recorded.roots.len(), 2);
        assert_eq!(recorded.shaders.len(), 1);
        let mut changed = Changed::new();
        for _ in 0..48 {
            recorded.observe(&mut changed, SHADOW, (0x50000, 174 << 20), PIECE + 128, PIECE + 192);
        }
        assert_eq!(changed.len(), 2);
        assert!(changed.iter().all(|(_, ranges)| ranges.spans() == [(PIECE, PIECE * 2)]));
        assert!(changed.keys().any(|(_, _, _, p)| p.root == Some(2) && p.shader.unwrap().indirect));
        assert!(!changed.keys().any(|(_, _, _, p)| p.root == Some(4)));
    }
}

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::rf_x9::enabled()
            && std::env::var_os("VICTORY_RF_X9_UAV_CAPTURE").is_some_and(|v| v == "1")
    })
}

impl Recorded {
    pub(super) fn clear(&mut self) {
        self.shaders.clear();
        self.roots.clear();
    }

    pub(super) fn shader(&mut self, shader: Shader) {
        self.shaders.insert(shader);
    }

    pub(super) fn root(&mut self, hash: u64, slot: u8, shadow: u64, from: usize, to: usize) {
        if from < to {
            self.roots.insert(Root { hash, slot, shadow, from, to });
        }
    }

    pub(super) fn collect<'a>(lists: impl IntoIterator<Item = &'a super::HostList>) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let mut all = Self::default();
        for list in lists {
            let list = list.deferred();
            all.shaders.extend(list.x9_uav.shaders.iter().copied());
            all.roots.extend(list.x9_uav.roots.iter().copied());
        }
        Some(all)
    }

    fn observe(&self, changed: &mut Changed, id: u64, (base, bytes): (u64, u64), lo: usize, hi: usize) {
        if lo >= hi {
            return;
        }
        let lo = lo / PIECE * PIECE;
        let hi = hi.saturating_add(PIECE - 1) / PIECE * PIECE;
        let hi = hi.min(bytes as usize);
        let mut record = |producer| changed.entry((id, base, bytes, producer)).or_default().insert(lo, hi);
        // Retain the unclassified candidates as well. A shader can use a
        // table, choose a view by index or mix root and address stores.
        for &shader in &self.shaders {
            record(Producer { shader: Some(shader), root: None, view: None });
        }
        if self.shaders.is_empty() {
            record(Producer { shader: None, root: None, view: None });
        }
        for root in self.roots.iter().filter(|r| r.shadow == id && r.from < hi && lo < r.to) {
            for &shader in self.shaders.iter().filter(|s| s.hash == root.hash) {
                record(Producer { shader: Some(shader), root: Some(root.slot), view: Some((root.from, root.to)) });
            }
        }
    }

    pub(super) fn compared<'a>(&self, changes: impl IntoIterator<Item = (&'a crate::shadow::Shadow, usize, usize)>) {
        let mut captured = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
        let map = captured.get_or_insert_with(BTreeMap::new);
        for (shadow, lo, hi) in changes {
            self.observe(map, shadow.id(), shadow.span(), lo, hi);
        }
    }
}

static CAPTURE: Mutex<Option<Changed>> = Mutex::new(None);

pub(super) fn save() {
    if !enabled() {
        return;
    }
    let Some(base) = std::env::var_os("LOCALAPPDATA") else { return };
    let dir = std::path::PathBuf::from(base).join("victory");
    let file = dir.join("x9-uav-capture.json");
    let tmp = dir.join(format!("x9-uav-capture-{}.tmp", std::process::id()));
    let mut json = String::from("{\"piece_bytes\":65536,\"observations\":[");
    {
        let captured = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(captured) = captured.as_ref() else { return };
        use std::fmt::Write;
        for (i, (&(id, base, bytes, producer), ranges)) in captured.iter().enumerate() {
            if i != 0 {
                json.push(',');
            }
            let _ = write!(json, "\n{{\"shadow\":{id},\"base\":{base},\"bytes\":{bytes},\"shader\":");
            if let Some(shader) = producer.shader {
                let _ = write!(json, "\"{:016x}\",\"indirect\":{},\"stores_by_address\":{}",
                    shader.hash, shader.indirect, shader.by_address);
            } else {
                json.push_str("null");
            }
            if let (Some(slot), Some((lo, hi))) = (producer.root, producer.view) {
                let _ = write!(json, ",\"binding\":\"root-uav\",\"root_uav\":{slot},\"view\":[{lo},{hi}]");
            } else {
                json.push_str(",\"binding\":\"unknown\"");
            }
            json.push_str(",\"changed\":[");
            for (j, &(lo, hi)) in ranges.spans().iter().enumerate() {
                if j != 0 {
                    json.push(',');
                }
                let _ = write!(json, "[{lo},{hi}]");
            }
            json.push_str("]}");
        }
    }
    json.push_str("\n]}\n");
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, json))
        .and_then(|()| std::fs::rename(&tmp, file))
    {
        tracing::warn!(%error, "rf-x9 could not save UAV capture");
    }
}
