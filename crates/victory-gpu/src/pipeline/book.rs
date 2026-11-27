//! The graphics pipelines a run used, written down: their shader containers
//! and graphics state, in the pipeline cache folder. The host pipeline
//! library keeps what a run made, but only a run that draws can ask for a
//! graphics pipeline, because only the title knows its state; with the
//! state written down `victory warm` makes them beforehand, so the first
//! run after a translator, driver or card change does not compile thousands
//! of pipelines while it draws.
//!
//! `known-pipelines.txt`: one line for each pipeline, `key stage:hash,...
//! state-in-hex`, with the key the early list uses; `book/<hash>.bin`: a
//! shader container (named by the first 16 hex digits of its BLAKE3, which
//! is checked on reading). `VICTORY_PIPELINE_BOOK=0`: nothing is written
//! down (what is there is still made by `victory warm`).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

const FILE: &str = "known-pipelines.txt";
const DIR: &str = "book";
/// Pipelines written down, at most.
const MAX: usize = 16_384;

struct Line {
    shaders: Vec<(String, String)>,
    state: String,
}

#[derive(Default)]
struct Book {
    lines: BTreeMap<String, Line>,
    /// Containers not on disk yet.
    unwritten: HashMap<String, Arc<Shader>>,
    changed: bool,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);
/// Pipelines written down in this run.
pub(crate) static NOTED: AtomicU64 = AtomicU64::new(0);

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PIPELINE_BOOK").map_or(true, |v| v != "0"))
}

fn parse(text: &str) -> BTreeMap<String, Line> {
    text.lines()
        .filter_map(|l| {
            let mut parts = l.split(' ');
            let (key, shaders, state) = (parts.next()?, parts.next()?, parts.next()?);
            let shaders = shaders
                .split(',')
                .map(|s| s.split_once(':').map(|(a, b)| (a.to_owned(), b.to_owned())))
                .collect::<Option<Vec<_>>>()?;
            Some((key.to_owned(), Line { shaders, state: state.to_owned() }))
        })
        .collect()
}

fn load() -> Book {
    let lines = super::library::dir()
        .and_then(|d| std::fs::read_to_string(d.join(FILE)).ok())
        .map(|t| parse(&t))
        .unwrap_or_default();
    Book { lines, ..Book::default() }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// A draw or dispatch requested the graphics pipeline `key`: written down
/// at the next save if the book does not have it.
pub(crate) fn note(key: &str, shaders: &[Arc<Shader>], state: &GraphicsState) {
    if !enabled() || super::library::dir().is_none() {
        return;
    }
    let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    let book = book.get_or_insert_with(load);
    if book.lines.len() >= MAX || book.lines.contains_key(key) {
        return;
    }
    let line = Line {
        shaders: shaders.iter().map(|s| (s.stage.to_owned(), s.hash.clone())).collect(),
        state: hex(&state.to_bytes()),
    };
    for s in shaders {
        book.unwritten.insert(s.hash.clone(), s.clone());
    }
    book.lines.insert(key.to_owned(), line);
    book.changed = true;
    NOTED.fetch_add(1, Ordering::Relaxed);
    super::library::mark_changed();
}

/// Writes what was added since the last save.
pub(crate) fn save() {
    let Some(dir) = super::library::dir() else { return };
    let (text, containers) = {
        let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(book) = book.as_mut().filter(|b| b.changed) else { return };
        book.changed = false;
        let text: String = book
            .lines
            .iter()
            .map(|(k, l)| {
                let shaders: Vec<String> = l.shaders.iter().map(|(a, b)| format!("{a}:{b}")).collect();
                format!("{k} {} {}\n", shaders.join(","), l.state)
            })
            .collect();
        (text, std::mem::take(&mut book.unwritten))
    };
    let folder = dir.join(DIR);
    if std::fs::create_dir_all(&folder).is_err() {
        return;
    }
    for (hash, shader) in containers {
        let path = folder.join(format!("{hash}.bin"));
        if !path.exists() {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, &shader.container).and_then(|()| std::fs::rename(&tmp, &path)).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    let tmp = dir.join(FILE).with_extension("tmp");
    if std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, dir.join(FILE))).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// What `victory warm` made of the book.
#[derive(Default, Debug)]
pub struct Made {
    /// Pipelines in the book.
    pub listed: usize,
    /// Pipelines the host made or loaded from its library.
    pub made: usize,
    /// Pipelines the host refused, or whose shaders or state are gone.
    pub refused: usize,
}

/// Makes the host pipelines of the book on `threads` threads (they go into
/// the library and the driver's cache); `done` is called after each.
pub(crate) fn make_all(threads: usize, done: &(dyn Fn() + Sync)) -> Made {
    let Some(dir) = super::library::dir() else { return Made::default() };
    let lines: Vec<Line> = std::fs::read_to_string(dir.join(FILE))
        .map(|t| parse(&t).into_values().collect())
        .unwrap_or_default();
    let listed = lines.len();
    let policy = super::ray_tracing::policy();
    let containers: Mutex<HashMap<String, Option<Arc<Shader>>>> = Mutex::default();
    let shader = |stage: &str, hash: &str| -> Option<Arc<Shader>> {
        let mut known = containers.lock().unwrap_or_else(|e| e.into_inner());
        known
            .entry(format!("{stage}:{hash}"))
            .or_insert_with(|| {
                let bytes = std::fs::read(dir.join(DIR).join(format!("{hash}.bin"))).ok()?;
                let full = blake3::hash(&bytes);
                (full.to_hex()[..16] == *hash).then(|| {
                    Arc::new(Shader {
                        stage: match stage {
                            "vertex" => "vertex",
                            "pixel" => "pixel",
                            "mesh" => "mesh",
                            _ => "compute",
                        },
                        ambient: crate::ambient::role(hash).or_else(|| crate::ambient::role_by_name(&bytes)),
                        hash: hash.to_owned(),
                        hair: super::hair_strands(&bytes),
                        container: bytes,
                        translated: OnceLock::new(),
                        ray_work: OnceLock::new(),
                    })
                })
            })
            .clone()
    };
    let (made, refused) = (AtomicU64::new(0), AtomicU64::new(0));
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.max(1).min(listed.max(1)) {
            s.spawn(|| {
                while let Some(line) = lines.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let shaders: Option<Vec<Arc<Shader>>> = line.shaders.iter().map(|(a, b)| shader(a, b)).collect();
                    let state = unhex(&line.state).and_then(|b| GraphicsState::from_bytes(&b));
                    let ok = match (shaders, state) {
                        (Some(shaders), Some(state)) => {
                            if policy.skips_passes()
                                && shaders.iter().any(|s| policy.omission(super::ray_tracing::work(s)).is_some())
                            {
                                // Left out in this ray-tracing mode, as at a run.
                                done();
                                continue;
                            }
                            HostPipeline::new(&shaders, Some(&state)).is_some()
                        }
                        _ => false,
                    };
                    (if ok { &made } else { &refused }).fetch_add(1, Ordering::Relaxed);
                    done();
                }
            });
        }
    });
    Made { listed, made: made.into_inner() as usize, refused: refused.into_inner() as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let lines = parse("k1 vertex:aa,pixel:bb 00ff\n");
        let l = &lines["k1"];
        assert_eq!(l.shaders, [("vertex".to_owned(), "aa".to_owned()), ("pixel".to_owned(), "bb".to_owned())]);
        assert_eq!(unhex(&l.state), Some(vec![0, 255]));
        assert_eq!(hex(&[0, 255]), "00ff");
    }

    #[test]
    fn state_round_trips_through_bytes() {
        let g = GraphicsState::default();
        let bytes = g.to_bytes();
        let back = GraphicsState::from_bytes(&bytes).expect("reads back");
        assert_eq!(back.describe(), g.describe());
        assert!(GraphicsState::from_bytes(&bytes[..bytes.len() - 1]).is_none());
    }
}
