//! The driver's table of static samplers, made in the same order every run,
//! so pipelines with static samplers can be kept in the pipeline library.
//!
//! The card's driver (NVIDIA) keeps the static samplers of every root
//! signature in one table of its own, one entry per distinct sampler state,
//! in the order those states are first made in the process, and a compiled
//! pipeline holds its samplers' places in that table. A pipeline loaded from
//! the library in another run therefore reads through whatever sits at the
//! places of the run that stored it (see `library::with_static_samplers`).
//! Measured with `examples/lut3d_probe.rs` (4070, driver 621.01,
//! 2026-11-27):
//! - stored after the composite's three samplers were made first, one root
//!   signature each, and loaded in a run that made them first in the same
//!   order and then ten others (`PRELOAD=1`, then `PRELOAD=1 SHIFT=10`):
//!   filtered, as made;
//! - loaded in a run that made them first in another order (`PRELOAD=lin`)
//!   or not first at all: unfiltered (another sampler);
//! - made first at another register and space, visible to the pixel stage
//!   only (`PRELOAD_ODD=1`): filtered. The table tells samplers apart by
//!   their state alone.
//!
//! So: every sampler state the title's pipelines ever had is kept in a file
//! beside the library ([`FILE`]), in the order first seen, and made right
//! after the device, before anything else makes a static sampler
//! ([`replay`]): those take the same places in every run. A pipeline whose
//! samplers are all among them is loaded from and stored in the library
//! under a name that also holds their places and states ([`admit`]); one
//! with a state new to the file is made, kept out of the library, and its
//! new states are added to the file: the next run makes them first too.
//!
//! `VICTORY_LIBRARY_STATIC_SAMPLERS=0`: pipelines with static samplers never
//! use the library (the way before); `=1`: they always do, without the
//! table (the way before that, wrong where the places moved).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

/// The sampler states, in the order they are made, beside the library.
pub(super) const FILE: &str = "static-samplers.bin";
const MAGIC: &[u8; 8] = b"VICSS001";
/// At most this many states are made first (the card's table holds 2,032;
/// the rest is left to the title's own and to ours).
const MAX: usize = 1024;

/// One static sampler's state: everything but its register, space and
/// visibility (the table does not tell those apart).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct State([u32; 10]);

impl State {
    pub(crate) fn of(s: &D3D12_STATIC_SAMPLER_DESC) -> Self {
        State([
            s.Filter.0 as u32,
            s.AddressU.0 as u32,
            s.AddressV.0 as u32,
            s.AddressW.0 as u32,
            s.MipLODBias.to_bits(),
            s.MaxAnisotropy,
            s.ComparisonFunc.0 as u32,
            s.BorderColor.0 as u32,
            s.MinLOD.to_bits(),
            s.MaxLOD.to_bits(),
        ])
    }

    fn desc(self) -> D3D12_STATIC_SAMPLER_DESC {
        let s = self.0;
        D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER(s[0] as i32),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE(s[1] as i32),
            AddressV: D3D12_TEXTURE_ADDRESS_MODE(s[2] as i32),
            AddressW: D3D12_TEXTURE_ADDRESS_MODE(s[3] as i32),
            MipLODBias: f32::from_bits(s[4]),
            MaxAnisotropy: s[5],
            ComparisonFunc: D3D12_COMPARISON_FUNC(s[6] as i32),
            BorderColor: D3D12_STATIC_BORDER_COLOR(s[7] as i32),
            MinLOD: f32::from_bits(s[8]),
            MaxLOD: f32::from_bits(s[9]),
            ShaderRegister: 0,
            RegisterSpace: 0,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    }

    fn bytes(self) -> [u8; 40] {
        let mut out = [0u8; 40];
        for (i, w) in self.0.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn from_bytes(b: &[u8]) -> Self {
        let mut w = [0u32; 10];
        for (i, v) in w.iter_mut().enumerate() {
            *v = u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap_or_default());
        }
        State(w)
    }
}

/// A record in the file: the state and a check of it (a torn or damaged
/// record ends the list there).
const RECORD: usize = 48;

fn record(state: State) -> [u8; RECORD] {
    let mut out = [0u8; RECORD];
    let bytes = state.bytes();
    out[..40].copy_from_slice(&bytes);
    let check = blake3::hash(&bytes);
    out[40..].copy_from_slice(&check.as_bytes()[..8]);
    out
}

/// The states a file holds, in order, up to the first bad record.
pub(crate) fn parse(file: &[u8]) -> Vec<State> {
    let Some(body) = file.strip_prefix(MAGIC.as_slice()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for r in body.chunks_exact(RECORD) {
        let state = State::from_bytes(&r[..40]);
        if blake3::hash(&r[..40]).as_bytes()[..8] != r[40..] {
            break;
        }
        out.push(state);
    }
    out
}

/// How a pipeline with static samplers may use the library.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Use {
    /// No static samplers: as any pipeline.
    Plain,
    /// All its samplers were made first, at these places: from and into the
    /// library, its name salted with this tag.
    Placed(u64),
    /// A sampler not made first: made, not from or into the library.
    Unplaced,
}

/// The sampler file is located beside the pipeline library, so this hint is
/// retained for callers that also configure the shader cache.
pub(crate) fn use_folder(_dir: PathBuf) {}

/// Fingerprint component for sampler ordering. The library's own folder is
/// authoritative for replay; include its bytes when available.
pub(crate) fn order() -> Vec<u8> {
    super::library::folder()
        .and_then(|dir| std::fs::read(dir.join(FILE)).ok())
        .unwrap_or_default()
}

/// Shader-cache notification; admission records states when the pipeline is
/// built, where the host sampler descriptions are available.
pub(crate) fn know(_samplers: &[victory_shader::emit::StaticSampler]) {}

struct Table {
    /// Places of the states made first ([`replay`]).
    placed: HashMap<State, u32>,
    /// States seen this run that are not placed, already in the file.
    noted: HashSet<State>,
    /// The number of states in the file.
    in_file: usize,
    path: Option<PathBuf>,
    /// The root signatures that made them, kept for the device's life.
    _made: Vec<ID3D12RootSignature>,
}

// SAFETY: D3D12 objects are free-threaded; the rest are plain data.
unsafe impl Send for Table {}

static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();

/// Whether the table decides (default), or `VICTORY_LIBRARY_STATIC_SAMPLERS`
/// says never (0) or always (1).
fn mode() -> Option<bool> {
    static MODE: OnceLock<Option<bool>> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_LIBRARY_STATIC_SAMPLERS").as_deref() {
        Ok("0") => Some(false),
        Ok("1") => Some(true),
        _ => None,
    })
}

/// Makes the states of [`FILE`] in `dir`, in their order, one root signature
/// each, on `device`: called right after the device is made, before anything
/// else makes a static sampler. Once; later calls do nothing.
pub(crate) fn replay(device: &ID3D12Device) {
    TABLE.get_or_init(|| {
        let path = super::library::folder().map(|d| d.join(FILE));
        let states = if mode().is_none() {
            path.as_deref().map(read).unwrap_or_default()
        } else {
            Vec::new()
        };
        let began = std::time::Instant::now();
        let mut made = Vec::with_capacity(states.len().min(MAX));
        let mut placed = HashMap::new();
        for state in states.iter().copied().take(MAX) {
            if placed.contains_key(&state) {
                continue;
            }
            match make(device, state) {
                Ok(root) => {
                    placed.insert(state, placed.len() as u32);
                    made.push(root);
                }
                Err(e) => {
                    // Places after a refused one would not be the stored
                    // runs' places: none are used past it.
                    tracing::warn!(error = %e, at = placed.len(), "a static sampler state refused; the later ones are not placed");
                    break;
                }
            }
        }
        if !states.is_empty() {
            tracing::info!(
                states = placed.len(),
                ms = began.elapsed().as_millis() as u64,
                "static sampler states made first, in the order of earlier runs (pipelines with them come from the library)"
            );
        }
        Mutex::new(Table {
            placed,
            noted: HashSet::new(),
            in_file: states.len(),
            path,
            _made: made,
        })
    });
}

/// The states in `path`; a torn end (a run that stopped while adding) is
/// cut off, so what is added next reads back.
fn read(path: &Path) -> Vec<State> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let states = parse(&bytes);
    let whole = MAGIC.len() + states.len() * RECORD;
    if !states.is_empty() && bytes.len() != whole {
        let cut = std::fs::OpenOptions::new().write(true).open(path).and_then(|f| f.set_len(whole as u64));
        if let Err(e) = cut {
            tracing::warn!(error = %e, "static sampler states: the torn end not cut off");
        }
    }
    states
}

fn make(device: &ID3D12Device, state: State) -> windows::core::Result<ID3D12RootSignature> {
    let sampler = state.desc();
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumStaticSamplers: 1,
                pStaticSamplers: &sampler,
                ..Default::default()
            },
        },
    };
    // SAFETY: the description points at a live sampler; the blob is read
    // while it lives.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)
    }
}

/// How a pipeline whose root signature has `samplers` may use the library;
/// states not in the file yet are added to it (made first from the next run
/// on).
pub(crate) fn admit(samplers: &[D3D12_STATIC_SAMPLER_DESC]) -> Use {
    if samplers.is_empty() {
        return Use::Plain;
    }
    match mode() {
        Some(true) => return Use::Placed(0),
        Some(false) => return Use::Unplaced,
        None => {}
    }
    let Some(table) = TABLE.get() else { return Use::Unplaced };
    let mut table = table.lock().unwrap_or_else(|e| e.into_inner());
    let mut tag = blake3::Hasher::new();
    let mut new = Vec::new();
    let mut unplaced = false;
    for s in samplers {
        let state = State::of(s);
        match table.placed.get(&state) {
            Some(&at) => {
                tag.update(&at.to_le_bytes());
                tag.update(&state.bytes());
            }
            None => {
                unplaced = true;
                if table.noted.insert(state) {
                    new.push(state);
                }
            }
        }
    }
    if !new.is_empty() {
        table.add(&new);
    }
    if unplaced {
        return Use::Unplaced;
    }
    let hash = tag.finalize();
    Use::Placed(u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap_or_default()))
}

impl Table {
    /// Appends `states` to the file (a new file starts with its mark).
    fn add(&mut self, states: &[State]) {
        let Some(path) = &self.path else { return };
        if self.in_file >= MAX {
            return;
        }
        let room = MAX - self.in_file;
        let mut bytes = Vec::with_capacity(8 + states.len() * RECORD);
        let fresh = !path.exists() || self.in_file == 0;
        if fresh {
            bytes.extend_from_slice(MAGIC);
        }
        for &s in states.iter().take(room) {
            bytes.extend_from_slice(&record(s));
        }
        let written = (|| -> std::io::Result<()> {
            if fresh {
                // A file whose records did not read (damaged) starts again.
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, &bytes)?;
                std::fs::rename(&tmp, path)
            } else {
                let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
                f.write_all(&bytes)
            }
        })();
        match written {
            Ok(()) => self.in_file += states.len().min(room),
            Err(e) => tracing::warn!(error = %e, "static sampler states not kept"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(bias: f32) -> State {
        State::of(&D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
            MipLODBias: bias,
            MaxLOD: 12.0,
            ShaderRegister: 3,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            ..Default::default()
        })
    }

    #[test]
    fn a_state_leaves_out_register_and_visibility() {
        let mut d = state(0.5).desc();
        d.ShaderRegister = 9;
        d.RegisterSpace = 2;
        assert_eq!(State::of(&d), state(0.5));
        assert_ne!(state(0.5), state(0.25));
    }

    #[test]
    fn the_file_reads_back_in_order_and_stops_at_a_torn_record() {
        let mut file = MAGIC.to_vec();
        for b in [0.0, 1.0, 2.0] {
            file.extend_from_slice(&record(state(b)));
        }
        assert_eq!(parse(&file), vec![state(0.0), state(1.0), state(2.0)]);
        // A torn last record, and a damaged middle one.
        let torn = &file[..file.len() - 5];
        assert_eq!(parse(torn), vec![state(0.0), state(1.0)]);
        let mut bad = file.clone();
        bad[8 + RECORD + 3] ^= 1;
        assert_eq!(parse(&bad), vec![state(0.0)]);
        assert!(parse(b"other").is_empty());
    }
}
