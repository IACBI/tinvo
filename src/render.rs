//! Offline (faster than real time) rendering.
//!
//! Rendering drives the very same [`Engine`] the audio thread uses, so an
//! export is bit-for-bit what the engine plays.
//!
//! The plugins a project names play through instances of the render's own,
//! made for offline rendering ([`PluginSource`]) before anything is
//! rendered: one that cannot load fails the render, named, and so does one
//! whose processing fails during it. The render runs on the thread that
//! made them, which plays CLAP's audio thread for them too, and serves
//! their requests between chunks.

use crate::clap::PluginRef;
use crate::clap::host::{Instance, Mode, PluginHost, Request, Role};
use crate::clap::scan::Catalog;
use crate::clip::ClipBank;
use crate::engine::transport::Transport;
use crate::engine::{BLOCK, Command, Edit, Engine, MAX_PATTERNS, RETIRE_QUEUE, Retired, Shared};
use crate::instrument::SampleMap;
use crate::project::{PluginPlace, Project, TrackData};
use crate::rt::spsc::{self, Consumer};
use std::path::Path;
use std::sync::Arc;

/// Checks that an export may write `out`: a `.wav`, `.mid` or `.midi` file
/// that is neither the project at `project_path` nor a sample or clip
/// `project` plays, which writing it would destroy. The error follows the path, as
/// in "song.tvo is the project itself".
pub fn check_destination(project: &Project, project_path: Option<&Path>, out: &Path) -> Result<(), String> {
    let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !["wav", "mid", "midi"].iter().any(|e| ext.eq_ignore_ascii_case(e)) {
        return Err("is not a .wav or .mid file".into());
    }
    check_overwrite(project, project_path, out)
}

/// Checks that writing `out` destroys nothing `project` needs: that it is
/// not a folder, a sample or clip file the project plays, nor (if given) the project at
/// `project_path`. A project's save passes no `project_path`, as it may
/// write over itself. The error follows the path.
pub fn check_overwrite(project: &Project, project_path: Option<&Path>, out: &Path) -> Result<(), String> {
    let out = match std::fs::canonicalize(out) {
        Ok(p) => p,
        // Nothing at the name: a new file.
        Err(_) if std::fs::symlink_metadata(out).is_err() => return Ok(()),
        // Something that does not resolve, such as a link to a file gone,
        // may still stand for a sample the editor holds.
        Err(e) => return Err(format!("cannot be followed ({e}); choose another name")),
    };
    if out.is_dir() {
        return Err("is a folder; choose a file name".into());
    }
    let same = |p: &Path| std::fs::canonicalize(p).is_ok_and(|p| p == out);
    if project_path.is_some_and(same) {
        return Err("is the project itself".into());
    }
    if project.tracks.iter().flat_map(TrackData::sample_paths).any(|s| same(Path::new(s))) {
        return Err("is a sample the project plays".into());
    }
    if project.tracks.iter().flat_map(TrackData::clip_paths).any(|s| same(Path::new(s))) {
        return Err("is a clip the project plays".into());
    }
    Ok(())
}

/// Checks that nothing is at `out`: a file named after another, as a stem
/// is after the mix, is never written over one there.
pub fn check_free(out: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(out) {
        Ok(_) => Err("exists already; stems replace no file, so move it away or name the mix otherwise".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Such as a name too long: refused before anything is written.
        Err(e) => Err(format!("cannot be written ({e}); name the mix otherwise")),
    }
}

/// What a render plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The song if it has entries, otherwise pattern A.
    Auto,
    /// One pattern on repeat.
    Pattern(u8),
    /// The whole song, start to end, ignoring the loop region.
    Song,
    /// The song's loop region.
    Region,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderOptions {
    pub sample_rate: u32,
    /// How many times the source plays.
    pub loops: u32,
    /// Upper bound on the release tail rendered after the last loop.
    pub max_tail_seconds: f32,
    pub source: Source,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { sample_rate: 48_000, loops: 4, max_tail_seconds: 10.0, source: Source::Auto }
    }
}

/// Level below which the tail is considered silent (-90 dBFS).
const TAIL_THRESHOLD: f32 = 3.2e-5;
/// The tail ends after this much continuous silence.
const TAIL_QUIET_SECONDS: f32 = 0.1;

/// The source a render actually plays: `Auto` resolved, and `Song` or
/// `Region` falling back to pattern A when there is nothing to play.
pub fn resolve(project: &Project, source: Source) -> Source {
    match source {
        Source::Auto | Source::Song if !project.song.is_empty() => Source::Song,
        Source::Region if project.active_loop().is_some() => Source::Region,
        Source::Pattern(p) if usize::from(p) < MAX_PATTERNS => source,
        _ => Source::Pattern(0),
    }
}

/// Clock pulses in one pass of `source`.
pub fn source_pulses(project: &Project, source: Source) -> u64 {
    match resolve(project, source) {
        Source::Song => project.song_pulses(),
        Source::Region => {
            let (a, b) = project.active_loop().unwrap_or((0, 0));
            project.entries_pulses(usize::from(a), usize::from(b))
        }
        Source::Pattern(p) => project.pattern_pulses(p),
        Source::Auto => unreachable!("resolved above"),
    }
}

/// Frames covered by `opts.loops` passes of the source, before the tail.
/// The song and a region follow the song's tempo points.
pub fn main_frames(project: &Project, opts: &RenderOptions) -> u64 {
    let sr = opts.sample_rate as f32;
    let entries = match resolve(project, opts.source) {
        Source::Song => Some((0, project.song.len().saturating_sub(1))),
        Source::Region => project.active_loop().map(|(a, b)| (usize::from(a), usize::from(b))),
        _ => None,
    };
    match entries {
        Some((a, b)) if project.has_tempo_points() => {
            (project.entries_frames(a, b, sr) * f64::from(opts.loops)).round() as u64
        }
        _ => {
            let sps = Transport::samples_per_step(project.tempo, sr);
            let sixteenths = (source_pulses(project, opts.source) * u64::from(opts.loops)) as f64 / 6.0;
            (sps * sixteenths).round() as u64
        }
    }
}

/// Checks, before anything is rendered, that the stereo file of `opts`'s
/// passes fits a WAV file in `format`. The tail after them is not known
/// yet: a file that only it makes too long still fails as it is written.
pub fn check_length(project: &Project, opts: &RenderOptions, format: crate::wav::SampleFormat) -> Result<(), String> {
    let frames = main_frames(project, opts);
    if crate::wav::fits(frames.saturating_mul(2), format) {
        return Ok(());
    }
    let minutes = frames / u64::from(opts.sample_rate.max(1)) / 60;
    Err(format!("{minutes} minutes of {} audio would not fit a WAV file; export fewer repeats", format.label()))
}

/// Frames the engine renders per call while exporting.
const CHUNK: usize = 4096;

/// Makes the plugin instances a render plays, on the thread that renders.
pub trait PluginSource {
    /// An instance of `plugin` as a `role`, made to render offline at
    /// `sample_rate` in calls of at most [`BLOCK`] frames, loading `state`
    /// first if given.
    fn make(
        &mut self,
        plugin: PluginRef,
        role: Role,
        sample_rate: f64,
        state: Option<&[u8]>,
    ) -> Result<Instance, String>;
    /// What the instances made have asked for since the last call
    /// ([`PluginHost::poll`]).
    fn poll(&mut self) -> Vec<Request>;
    /// The plugin's name, for messages.
    fn name(&self, plugin: PluginRef) -> String;
}

/// Makes no instances: for projects without plugins.
pub struct NoPlugins;

impl PluginSource for NoPlugins {
    fn make(&mut self, _: PluginRef, _: Role, _: f64, _: Option<&[u8]>) -> Result<Instance, String> {
        Err("no plugin catalog was read".into())
    }

    fn poll(&mut self) -> Vec<Request> {
        Vec::new()
    }

    fn name(&self, plugin: PluginRef) -> String {
        plugin.id().to_string()
    }
}

/// The plugins a scan found, through a host of its own on this thread.
pub struct CatalogPlugins {
    host: PluginHost,
    catalog: Catalog,
}

impl CatalogPlugins {
    /// # Safety
    /// As [`PluginHost::create`], for every file in `catalog`: their code
    /// runs in this process, trusted, and this is made (and, not being
    /// `Send`, used) on the thread CLAP calls main.
    pub unsafe fn new(catalog: Catalog) -> Self {
        Self { host: PluginHost::new(), catalog }
    }
}

impl PluginSource for CatalogPlugins {
    fn make(
        &mut self,
        plugin: PluginRef,
        role: Role,
        sample_rate: f64,
        state: Option<&[u8]>,
    ) -> Result<Instance, String> {
        let (file, _) = self.catalog.find(plugin.id()).ok_or("not found in the plugin catalog")?;
        // SAFETY: a file of the catalog, on the thread that made this, as
        // its maker promised.
        unsafe { self.host.create_with_state(file, plugin.id(), role, Mode::Offline, sample_rate, BLOCK, state) }
    }

    fn poll(&mut self) -> Vec<Request> {
        self.host.poll()
    }

    fn name(&self, plugin: PluginRef) -> String {
        self.catalog.find(plugin.id()).map_or_else(|| plugin.id().to_string(), |(_, p)| p.name.clone())
    }
}

/// The instances a render plays, by token, for its messages.
struct Playing {
    tokens: Vec<(u64, PluginRef)>,
    back: Consumer<Retired>,
}

impl Playing {
    /// Puts an instance in every plugin place of `project`, each starting
    /// from the state the project keeps for it.
    fn attach(
        engine: &mut Engine,
        project: &Project,
        sample_rate: u32,
        plugins: &mut dyn PluginSource,
    ) -> Result<Playing, String> {
        let (tx, back) = spsc::channel(RETIRE_QUEUE);
        engine.attach_retirement(tx);
        let mut playing = Playing { tokens: Vec::new(), back };
        for (place, plugin) in project.plugin_places() {
            let state = project.plugin_state(place).map(|s| &s[..]);
            let made = plugins.make(plugin, place.role(), f64::from(sample_rate), state);
            let name = || plugins.name(plugin);
            let instance = made.map_err(|e| format!("{} cannot load: {e}", name()))?;
            playing.tokens.push((instance.token(), plugin));
            let instance = Box::new(instance);
            match place {
                PluginPlace::Track(t) => engine.set_plugin(usize::from(t), instance),
                PluginPlace::Slot(chain, slot) => engine.set_slot_plugin(chain, slot, instance),
            }
            if std::iter::from_fn(|| playing.back.pop()).any(|r| matches!(r, Retired::Plugin(_))) {
                return Err(format!("{} cannot play there", name()));
            }
        }
        // Every path in time from the first frame.
        engine.settle_latency();
        Ok(playing)
    }

    /// Serves what the instances asked for since the last call: a restart
    /// is done in place; a failure fails the render, and so does a restart
    /// that makes it come later or sooner, which the frames already
    /// rendered could not be lined up with.
    fn serve(&mut self, engine: &mut Engine, plugins: &mut dyn PluginSource) -> Result<(), String> {
        for request in plugins.poll() {
            // What its parameters and state did is no render's concern.
            let (Request::Restart(token) | Request::Failed(token)) = request else { continue };
            let name = self.tokens.iter().find(|t| t.0 == token).map_or_else(String::new, |t| plugins.name(t.1));
            if let Request::Failed(_) = request {
                return Err(format!("{name} failed while rendering"));
            }
            engine.handle(Command::Lend(token));
            while let Some(r) = self.back.pop() {
                if let Retired::Lent(mut i) = r {
                    let latency = i.latency();
                    i.restart().map_err(|e| format!("{name} could not restart: {e}"))?;
                    if i.latency() != latency {
                        return Err(format!("{name} changed its latency while rendering"));
                    }
                    engine.handle(Command::Return(i));
                }
            }
        }
        while self.back.pop().is_some() {}
        Ok(())
    }
}

/// Renders the project as interleaved stereo, handing it to `sink` block by
/// block, and returns the number of frames rendered. Memory use does not
/// depend on the length of the render. `samples` holds what each track
/// plays (see [`Project::sample_maps`]); missing entries play silence, as
/// clips whose files `clips` does not hold do. Every
/// plugin place gets an instance from `plugins` before anything is
/// rendered. A `sink` error, a plugin that cannot load or fails, or plugins
/// on one path later together than is made up for, stop the render, and
/// the error is returned.
pub fn render_into(
    project: &Project,
    samples: &[Option<Arc<SampleMap>>],
    clips: &ClipBank,
    opts: &RenderOptions,
    plugins: &mut dyn PluginSource,
    sink: &mut dyn FnMut(&[f32]) -> Result<(), String>,
) -> Result<u64, String> {
    let sr = opts.sample_rate;
    let shared = Arc::new(Shared::new());
    let mut engine = Engine::with_edits(sr as f32, &project.to_edits(), Arc::clone(&shared), None);
    for (track, sample) in samples.iter().enumerate() {
        engine.set_samples(track, sample.clone());
    }
    for (track, list) in project.clip_lists(clips).into_iter().enumerate() {
        engine.set_clips(track, list);
    }
    let mut playing = Playing::attach(&mut engine, project, sr, plugins)?;
    if shared.latency_over.load(std::sync::atomic::Ordering::Relaxed) {
        let ms = crate::clap::host::MAX_LATENCY_SECONDS * 1000.0;
        return Err(format!("the plugins on one path come later together than the {ms} ms an export makes up for"));
    }
    match resolve(project, opts.source) {
        Source::Song => {
            engine.apply(Edit::SetLoop(None));
            engine.handle(Command::PlaySong { entry: 0 });
        }
        Source::Region => {
            let (a, _) = project.active_loop().unwrap_or((0, 0));
            engine.handle(Command::PlaySong { entry: a });
        }
        Source::Pattern(p) => {
            engine.handle(Command::Cue(p));
            engine.handle(Command::Play);
        }
        Source::Auto => unreachable!("resolved above"),
    }
    let mut buf = vec![0.0f32; CHUNK * 2];
    let main = main_frames(project, opts);
    // The master limiter delays the output; render that much ahead and drop
    // it, so the file still starts on the first beat. The transport stops
    // at the same musical point either way.
    // Read once: a lane can switch the limiter during the preroll.
    let latency = engine.latency() as u64;
    // Frames played, the preroll's first: the transport stops after `main`
    // of them, however late the output comes.
    let (total, mut at, mut stopped) = (latency + main, 0u64, false);
    while at < total {
        if !stopped && at == main {
            engine.handle(Command::Stop);
            stopped = true;
        }
        let mut end = (at + CHUNK as u64).min(total);
        if !stopped {
            end = end.min(main);
        }
        if at < latency {
            end = end.min(latency);
        }
        let n = (end - at) as usize;
        engine.process(&mut buf[..n * 2]);
        playing.serve(&mut engine, plugins)?;
        if at >= latency {
            sink(&buf[..n * 2])?;
        }
        at = end;
    }
    if !stopped {
        engine.handle(Command::Stop);
    }
    let mut done = main;

    // The tail ends after `quiet_needed` frames of silence once the effects
    // hold nothing more (a long delay's echo can follow a quiet spell). The
    // silence is not part of the result: quiet blocks wait in `held` until
    // sound follows.
    let max_tail = (opts.max_tail_seconds.max(0.0) * sr as f32) as usize;
    let quiet_needed = (TAIL_QUIET_SECONDS * sr as f32) as usize;
    let mut held: Vec<f32> = Vec::new();
    let mut tail = 0usize;
    while tail < max_tail && (held.len() / 2 < quiet_needed || !engine.effects_idle()) {
        let n = 512.min(max_tail - tail);
        let b = &mut buf[..n * 2];
        engine.process(b);
        playing.serve(&mut engine, plugins)?;
        tail += n;
        if b.iter().all(|x| x.abs() < TAIL_THRESHOLD) {
            held.extend_from_slice(b);
        } else {
            if !held.is_empty() {
                sink(&held)?;
                done += (held.len() / 2) as u64;
                held.clear();
            }
            sink(b)?;
            done += n as u64;
        }
    }
    Ok(done)
}

/// [`render_into`] collected in memory, for a project without plugins.
#[cfg(test)]
pub fn render(project: &Project, samples: &[Option<Arc<SampleMap>>], opts: &RenderOptions) -> Vec<f32> {
    render_with(project, samples, &ClipBank::new(), opts)
}

/// [`render`], with the clips' audio in `clips`.
#[cfg(test)]
pub fn render_with(
    project: &Project,
    samples: &[Option<Arc<SampleMap>>],
    clips: &ClipBank,
    opts: &RenderOptions,
) -> Vec<f32> {
    let mut out = Vec::new();
    let mut sink = |b: &[f32]| {
        out.extend_from_slice(b);
        Ok(())
    };
    render_into(project, samples, clips, opts, &mut NoPlugins, &mut sink).expect("collecting cannot fail");
    out
}

/// The tracks that are heard: not muted, soloed if any track is, and not
/// in a muted group.
pub fn audible_tracks(project: &Project) -> Vec<usize> {
    let any_solo = project.tracks.iter().any(|t| t.solo);
    let group_muted = |g: Option<u8>| g.and_then(|g| project.groups.get(usize::from(g))).is_some_and(|g| g.mute);
    let heard = |t: &TrackData| !t.mute && (!any_solo || t.solo) && !group_muted(t.group);
    (0..project.tracks.len()).filter(|&i| heard(&project.tracks[i])).collect()
}

/// What one stem holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stem {
    /// A track that goes to the master.
    Track(usize),
    /// A group bus, with every heard track that goes to it.
    Group(u8),
}

impl Stem {
    /// How its file is told apart: the track's number (from 1) or `g` and
    /// the group's, then the name, made safe for a file name.
    pub fn file_part(self, project: &Project) -> String {
        let (number, name) = match self {
            Stem::Track(i) => (format!("{:02}", i + 1), &project.tracks[i].name),
            Stem::Group(g) => (format!("g{}", u32::from(g) + 1), &project.groups[usize::from(g)].name),
        };
        let name: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
        format!("{number}-{name}")
    }
}

/// The stems of a project: one per heard track that goes to the master,
/// then one per group that a heard track goes to. Together they hold every
/// heard track once.
pub fn stems(project: &Project) -> Vec<Stem> {
    let heard = audible_tracks(project);
    let group_of = |i: usize| project.tracks[i].group;
    let tracks = heard.iter().filter(|&&i| group_of(i).is_none()).map(|&i| Stem::Track(i));
    let groups = (0..project.groups.len() as u8).filter(|&g| heard.iter().any(|&i| group_of(i) == Some(g)));
    tracks.chain(groups.map(Stem::Group)).collect()
}

/// The project as heard in stem `stem`. The tracks it does not hold are
/// muted, not removed: they still use the chance generator and choke open
/// hats exactly as in the mix, so the stems add up to it. A stem carries
/// its own share of the delay and reverb returns, and is taken before the
/// master's insert chain and limiter. A group's stem has the group's own
/// chain on it.
pub fn stem_project(project: &Project, stem: Stem) -> Project {
    let heard = audible_tracks(project);
    let mut solo = project.clone();
    for (j, t) in solo.tracks.iter_mut().enumerate() {
        t.mute = match stem {
            Stem::Track(i) => j != i,
            Stem::Group(g) => t.group != Some(g) || !heard.contains(&j),
        };
        t.solo = false;
    }
    // A group none of the stem's tracks feed is still played, its inserts
    // too, and a plugin among them can sound from silence.
    for (g, group) in solo.groups.iter_mut().enumerate() {
        let kept = match stem {
            Stem::Track(i) => project.tracks[i].group == Some(g as u8),
            Stem::Group(s) => usize::from(s) == g,
        };
        group.mute |= !kept;
    }
    solo.mix[crate::mix::mp::LIMITER] = 0.0;
    solo.master_chain = Default::default();
    // A lane could switch the limiter back on, or automates a master
    // effect no longer there.
    let limiter = crate::engine::LaneTarget::Mix(crate::mix::mp::LIMITER as u8);
    for lane in solo.tracks.iter_mut().flat_map(|t| t.lanes.iter_mut()) {
        let master = matches!(
            lane.target,
            Some(
                crate::engine::LaneTarget::MasterSlot { .. }
                    | crate::engine::LaneTarget::Plugin { at: crate::engine::PluginAt::MasterSlot(_), .. }
            )
        );
        if lane.target == Some(limiter) || master {
            *lane = crate::project::LaneData::new(None, false);
        }
    }
    solo
}

/// Renders each of the [`stems`] on its own (with the master level
/// applied). All are padded to the same length, so they line up when
/// imported elsewhere; added together they make the mix. The export
/// streams each stem instead ([`render_into`]).
#[cfg(test)]
pub fn render_stems(
    project: &Project,
    samples: &[Option<Arc<SampleMap>>],
    opts: &RenderOptions,
) -> Vec<(Stem, Vec<f32>)> {
    let mut stems: Vec<(Stem, Vec<f32>)> =
        stems(project).into_iter().map(|s| (s, render(&stem_project(project, s), samples, opts))).collect();
    let len = stems.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
    for (_, s) in &mut stems {
        s.resize(len, 0.0);
    }
    stems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Division, Step};
    use crate::instrument::InstrumentKind;

    #[test]
    fn an_export_too_long_for_a_wav_file_is_refused_before_it_starts() {
        // A bar of 16ths at 120 BPM: 2 s, 288 000 bytes in 24-bit stereo.
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        let opts = |loops| RenderOptions { loops, source: Source::Pattern(0), ..Default::default() };
        let format = crate::wav::SampleFormat::Pcm24;
        assert_eq!(check_length(&p, &opts(7000), format), Ok(()));
        let e = check_length(&p, &opts(8000), format).unwrap_err();
        assert_eq!(e, "266 minutes of 24-bit audio would not fit a WAV file; export fewer repeats");
        assert!(crate::wav::fits(u64::from(u32::MAX) / 4 - 20, crate::wav::SampleFormat::Float32));
        assert!(!crate::wav::fits(u64::from(u32::MAX) / 4, crate::wav::SampleFormat::Float32));
    }

    #[test]
    fn an_export_writes_only_a_new_or_exported_file() {
        let dir = std::env::temp_dir().join(format!("tinvo-dest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let song = dir.join("song.tvo");
        let kick = dir.join("kick.wav");
        let old = dir.join("old.wav");
        for f in [&song, &kick, &old] {
            std::fs::write(f, "x").unwrap();
        }
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
        p.tracks[0].sample = Some(kick.display().to_string());
        let check = |out: &Path| check_destination(&p, Some(&song), out);
        assert_eq!(check(&song), Err("is not a .wav or .mid file".into()));
        assert_eq!(check(&dir.join("notes.txt")), Err("is not a .wav or .mid file".into()));
        assert_eq!(check(&dir.join("x")), Err("is not a .wav or .mid file".into()));
        assert_eq!(check(&dir.join("sub").join("..").join("kick.wav")), Err("is a sample the project plays".into()));
        assert_eq!(check(&old), Ok(()), "an earlier export is written over");
        assert_eq!(check(&dir.join("new.MID")), Ok(()));
        assert_eq!(check(&dir.join("new.WAV")), Ok(()));
        assert_eq!(check(&dir.join("new.midi")), Ok(()));
        std::fs::create_dir(dir.join("folder.wav")).unwrap();
        assert_eq!(check(&dir.join("folder.wav")), Err("is a folder; choose a file name".into()));
        // A link to a file that is gone is not taken for a new file.
        #[cfg(unix)]
        {
            let link = dir.join("link.wav");
            std::os::unix::fs::symlink(dir.join("gone.wav"), &link).unwrap();
            assert!(check(&link).is_err_and(|e| e.starts_with("cannot be followed")));
        }
        // A project named like a WAV file is still the project.
        let odd = dir.join("odd.wav");
        std::fs::write(&odd, "x").unwrap();
        assert_eq!(check_destination(&p, Some(&odd), &odd), Err("is the project itself".into()));
        // A stem's name is free or refused.
        assert_eq!(check_free(&dir.join("free-01-Kick.wav")), Ok(()));
        assert!(check_free(&song).unwrap_err().starts_with("exists already"));
        let long = dir.join(format!("{}.wav", "a".repeat(300)));
        assert!(check_free(&long).unwrap_err().starts_with("cannot be written"), "a name too long");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn demo_renders_deterministically() {
        let p = Project::demo();
        let opts = RenderOptions { loops: 1, ..Default::default() };
        let a = render(&p, &[], &opts);
        let b = render(&p, &[], &opts);
        assert_eq!(a, b);
        let main = main_frames(&p, &opts) as usize;
        assert!(a.len() / 2 >= main);
        // The demo's delay repeats every 0.49 s at 35 % feedback: its last
        // repeat above the tail's threshold (-90 dB) ends about 2.3 s on.
        assert!(a.len() / 2 < main + 48_000 * 3, "tail too long: {}", a.len() / 2 - main);
        let peak = a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "peak {peak}");
        assert!(a.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn length_follows_tempo_loops_and_source() {
        let mut p = Project::new();
        p.tempo = 120.0;
        let opts = |loops, source| RenderOptions { loops, source, ..Default::default() };
        assert_eq!(main_frames(&p, &opts(1, Source::Auto)), 96_000);
        assert_eq!(main_frames(&p, &opts(3, Source::Auto)), 288_000);
        // An empty project renders exactly the pattern, no tail.
        let out = render(&p, &[], &opts(2, Source::Auto));
        assert_eq!(out.len(), 192_000 * 2);
        assert!(out.iter().all(|&x| x == 0.0));
        // Song and region follow each pattern's length.
        p.pattern_lens[1] = 8;
        p.pattern_lens[2] = 4;
        p.song = vec![0, 1, 2, 1];
        p.loop_region = Some((1, 2));
        assert_eq!(resolve(&p, Source::Auto), Source::Song);
        assert_eq!(main_frames(&p, &opts(1, Source::Song)), 36 * 6000);
        assert_eq!(main_frames(&p, &opts(2, Source::Region)), 2 * 12 * 6000);
        assert_eq!(main_frames(&p, &opts(1, Source::Pattern(2))), 4 * 6000);
        // Steps of other lengths: eight triplet eighths are 8000 frames each.
        p.divisions[2] = Division::EighthTriplet;
        assert_eq!(main_frames(&p, &opts(1, Source::Pattern(2))), 4 * 8000);
        assert_eq!(main_frames(&p, &opts(1, Source::Song)), 32 * 6000 + 4 * 8000);
        p.divisions[2] = Division::Sixteenth;
        // A region past the end of the song is not active.
        p.loop_region = Some((2, 9));
        assert_eq!(resolve(&p, Source::Region), Source::Pattern(0));
    }

    /// Frames where sound starts after at least 1000 silent frames.
    fn onsets(out: &[f32]) -> Vec<usize> {
        let left: Vec<f32> = out.chunks(2).map(|f| f[0].abs()).collect();
        let mut v = Vec::new();
        let mut silent_run = usize::MAX;
        for (i, &x) in left.iter().enumerate() {
            if x > 1e-9 {
                if silent_run >= 1000 {
                    v.push(i);
                }
                silent_run = 0;
            } else {
                silent_run = silent_run.saturating_add(1);
            }
        }
        v
    }

    #[test]
    fn the_tail_keeps_an_echo_that_follows_a_quiet_spell() {
        use crate::mix::mp;
        // One short hat at 60 BPM into a half-note delay: the echo comes
        // 2 s after the hit, long after the pattern's 1 s is over.
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::HatClosed)).unwrap();
        p.apply(Edit::SetPatternLength { pattern: 0, len: 4 }).unwrap();
        p.tracks[0].steps[0][0] = Some(Step::new(60, 127, 1));
        p.tempo = 60.0;
        let delay_send = InstrumentKind::HatClosed.params().len() + crate::strip::sp::COUNT + crate::mix::send::DELAY;
        p.apply(Edit::SetParam { track: 0, index: delay_send as u8, value: 0.0 }).unwrap();
        p.mix[mp::DELAY_TIME] = (crate::mix::DIVISIONS.len() - 1) as f32;
        p.mix[mp::DELAY_FEEDBACK] = 0.0;
        let opts = RenderOptions { loops: 1, source: Source::Pattern(0), ..Default::default() };
        let out = render(&p, &[], &opts);
        assert!(out.len() / 2 > 96_000, "{} frames", out.len() / 2);
        let echo = out[2 * 95_000..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(echo > 0.01, "{echo}");
    }

    /// A short closed hat on step 0 of pattern A and step 2 of pattern B
    /// (4 steps each).
    fn two_patterns() -> Project {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::HatClosed)).unwrap();
        for pattern in [0, 1] {
            p.apply(Edit::SetPatternLength { pattern, len: 4 }).unwrap();
        }
        p.tracks[0].steps[0][0] = Some(Step::new(60, 127, 1));
        p.tracks[0].steps[1][2] = Some(Step::new(60, 127, 1));
        p
    }

    #[test]
    fn song_and_region_play_patterns_in_order() {
        let mut p = two_patterns();
        let quick = |p: &Project, source| {
            let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source, ..Default::default() };
            onsets(&render(p, &[], &opts))
        };
        // Steps are 6000 frames at 120 BPM.
        assert_eq!(quick(&p, Source::Pattern(1)), [12_000]);
        p.song = vec![1, 0, 0, 1];
        assert_eq!(quick(&p, Source::Song), [12_000, 24_000, 48_000, 84_000]);
        // The region repeats entries 1..=2 (A, A); the song plays straight
        // through regardless.
        p.loop_region = Some((1, 2));
        let opts = RenderOptions { loops: 2, max_tail_seconds: 0.0, source: Source::Region, ..Default::default() };
        assert_eq!(onsets(&render(&p, &[], &opts)), [0, 24_000, 48_000, 72_000]);
        assert_eq!(quick(&p, Source::Song), [12_000, 24_000, 48_000, 84_000]);
    }

    #[test]
    fn a_song_with_tempo_points_renders_as_long_as_the_engine_plays_it() {
        let mut p = two_patterns();
        p.song = vec![0, 0, 0];
        p.tempo_points[0] = Some(120.0);
        p.tempo_points[2] = Some(60.0);
        p.tempo_ramp = true;
        let opts = |loops| RenderOptions { loops, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let one = main_frames(&p, &opts(1));
        assert_eq!(one, p.entries_frames(0, 2, 48_000.0).round() as u64);
        let out = render(&p, &[], &opts(2));
        assert_eq!(out.len() as u64 / 2, main_frames(&p, &opts(2)));
        // The second pass starts where the first one's length says.
        let hits = onsets(&out);
        assert_eq!(hits.len(), 6, "{hits:?}");
        assert!(hits[3].abs_diff(one as usize) <= 1, "{hits:?} vs {one}");
        for k in 0..3 {
            assert!(hits[k + 3].abs_diff(hits[k] + one as usize) <= 1, "{hits:?}");
        }
    }

    #[test]
    fn exports_play_each_track_s_part() {
        let mut p = two_patterns();
        p.song = vec![0, 0, 0];
        p.tracks[0].parts[1] = crate::engine::Part::Pattern(1);
        p.tracks[0].parts[2] = crate::engine::Part::Rest;
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        // Steps are 6000 frames: A on entry 0, B's step 2 on entry 1, a rest.
        assert_eq!(onsets(&render(&p, &[], &opts)), [0, 36_000]);
        // Stems of a song with parts still add up to the mix.
        let mut q = Project::demo();
        q.mix[crate::mix::mp::LIMITER] = 0.0;
        q.tracks[1].parts[1] = crate::engine::Part::Rest;
        q.tracks[5].parts[0] = crate::engine::Part::Pattern(1);
        let mix = render(&q, &[], &opts);
        let stems = render_stems(&q, &[], &opts);
        let worst = (0..mix.len())
            .map(|k| (stems.iter().map(|(_, s)| s[k]).sum::<f32>() - mix[k]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "stems differ from the mix by {worst}");
        assert_ne!(
            mix,
            render(&Project { tracks: Project::demo().tracks, ..q.clone() }, &[], &opts),
            "the parts are heard"
        );
    }

    #[test]
    fn stems_with_insert_chains_add_up_to_the_mix_before_the_master_s() {
        use crate::engine::ChainId;
        use crate::insert::EffectKind;
        let mut p = Project::demo();
        p.mix[crate::mix::mp::LIMITER] = 0.0;
        let edits = [
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            Edit::SetTrackGroup { track: 1, group: Some(0) },
            Edit::SetSlot { chain: ChainId::Group(0), slot: 0, kind: Some(EffectKind::Delay) },
            Edit::SetSlotParam { chain: ChainId::Group(0), slot: 0, index: 5, value: 0.8 },
            Edit::SetSlot { chain: ChainId::Track(3), slot: 2, kind: Some(EffectKind::Chorus) },
            // A gate on one track: the stems are lined up as the mix is.
            Edit::SetSlot { chain: ChainId::Track(2), slot: 0, kind: Some(EffectKind::Gate) },
        ];
        for e in edits {
            p.apply(e).unwrap();
        }
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let sum = |p: &Project| {
            let stems = render_stems(p, &[], &opts);
            assert!(stems.iter().any(|(s, _)| *s == Stem::Group(0)));
            (0..stems[0].1.len()).map(|k| stems.iter().map(|(_, s)| s[k]).sum::<f32>()).collect::<Vec<_>>()
        };
        let mix = render(&p, &[], &opts);
        let stems = sum(&p);
        assert_eq!(stems.len(), mix.len());
        let worst = stems.iter().zip(&mix).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "stems differ from the mix by {worst}");
        // A master effect, and a lane on it, are left out of every stem.
        let plain = p.clone();
        p.apply(Edit::SetSlot { chain: ChainId::Master, slot: 0, kind: Some(EffectKind::Drive) }).unwrap();
        p.apply(Edit::SetSlotParam { chain: ChainId::Master, slot: 0, index: 1, value: 24.0 }).unwrap();
        let lane = crate::engine::LaneTarget::MasterSlot { slot: 0, kind: EffectKind::Drive, index: 0 };
        p.apply(Edit::SetLane { track: 2, lane: 0, target: Some(lane), ramp: false }).unwrap();
        assert_ne!(render(&p, &[], &opts), mix);
        assert_eq!(sum(&p), sum(&plain));
    }

    #[test]
    fn an_export_through_a_gate_starts_on_time() {
        use crate::engine::ChainId;
        use crate::insert::EffectKind;
        let mut p = two_patterns();
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        p.tracks[1].steps[0][1] = Some(Step::new(60, 100, 1));
        p.tracks[1].steps[1][3] = Some(Step::new(60, 100, 1));
        p.song = vec![0, 1];
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.5, source: Source::Song, ..Default::default() };
        let plain = render(&p, &[], &opts);
        // An open gate on a track only delays it, and the export takes its
        // look-ahead off the front as it does the limiter's.
        let range = EffectKind::Gate.params().iter().position(|s| s.id == "gate_range").unwrap() as u8;
        for chain in [ChainId::Track(1), ChainId::Master] {
            let mut q = p.clone();
            q.apply(Edit::SetSlot { chain, slot: 4, kind: Some(EffectKind::Gate) }).unwrap();
            q.apply(Edit::SetSlotParam { chain, slot: 4, index: range, value: 0.0 }).unwrap();
            assert_eq!(render(&q, &[], &opts), plain, "{chain:?}");
        }
    }

    #[test]
    fn a_stem_has_no_limiter_even_where_a_lane_turns_it_on() {
        use crate::mix::mp;
        let mut p = two_patterns();
        p.mix[mp::LIMITER] = 0.0;
        p.mix[mp::LIMITER_CEILING] = -30.0;
        let plain = p.clone();
        p.apply(Edit::SetLane {
            track: 0,
            lane: 1,
            target: Some(crate::engine::LaneTarget::Mix(mp::LIMITER as u8)),
            ramp: false,
        })
        .unwrap();
        p.apply(Edit::SetPoint { track: 0, lane: 1, pattern: 0, step: 0, value: Some(1.0) }).unwrap();
        let stem = stem_project(&p, Stem::Track(0));
        assert!(stem.tracks.iter().all(|t| t.lanes.iter().all(|l| l.target.is_none())));
        // The hat goes well past the -30 dB ceiling, unlimited.
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Pattern(0), ..Default::default() };
        let out = render(&stem, &[], &opts);
        assert_eq!(out, render(&stem_project(&plain, Stem::Track(0)), &[], &opts));
        assert!(out.iter().fold(0.0f32, |m, x| m.max(x.abs())) > 0.1);
    }

    #[test]
    fn stems_add_up_to_the_mix() {
        let mut p = Project::demo();
        p.tracks[4].mute = true; // a muted track gets no stem
        // Stems are taken before the master limiter, so they add up to the
        // mix without it.
        p.mix[crate::mix::mp::LIMITER] = 0.0;
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let mix = render(&p, &[], &opts);
        let stems = render_stems(&p, &[], &opts);
        assert_eq!(stems.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [0, 1, 2, 3, 5].map(Stem::Track));
        for (_, s) in &stems {
            assert_eq!(s.len(), mix.len());
        }
        let worst = (0..mix.len())
            .map(|k| (stems.iter().map(|(_, s)| s[k]).sum::<f32>() - mix[k]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "stems differ from the mix by {worst}");
    }

    #[test]
    fn a_group_gets_one_stem_and_the_stems_still_add_up() {
        use crate::group::group_param;
        let param = |id: &str| (0..).find(|&i| group_param(i).is_some_and(|s| s.id == id)).unwrap() as u8;
        let mut p = Project::demo();
        p.mix[crate::mix::mp::LIMITER] = 0.0;
        let edits = [
            Edit::InsertGroup(0),
            Edit::InsertGroup(1),
            Edit::InsertGroup(2),
            // Drums, squeezed hard and sent to the reverb: neither is linear
            // in a member alone, so only the whole group adds up.
            Edit::SetTrackGroup { track: 0, group: Some(1) },
            Edit::SetTrackGroup { track: 1, group: Some(1) },
            Edit::SetTrackGroup { track: 2, group: Some(1) },
            Edit::SetGroupParam { group: 1, index: param("comp_threshold"), value: -30.0 },
            Edit::SetGroupParam { group: 1, index: param("comp_ratio"), value: 8.0 },
            Edit::SetGroupParam { group: 1, index: param("send_reverb"), value: -6.0 },
            Edit::SetGroupPan { group: 1, pan: -0.5 },
            // A muted group: its track is not heard and gets no stem.
            Edit::SetTrackGroup { track: 3, group: Some(2) },
            Edit::SetGroupMute { group: 2, mute: true },
            Edit::SetMute { track: 4, mute: true },
        ];
        for e in edits {
            p.apply(e).unwrap();
        }
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let mix = render(&p, &[], &opts);
        let stems = render_stems(&p, &[], &opts);
        let which: Vec<Stem> = stems.iter().map(|(s, _)| *s).collect();
        assert_eq!(which, [Stem::Track(5), Stem::Group(1)], "group 0 has no track, group 2 is muted");
        assert_eq!(audible_tracks(&p), [0, 1, 2, 5]);
        let worst = (0..mix.len())
            .map(|k| (stems.iter().map(|(_, s)| s[k]).sum::<f32>() - mix[k]).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "stems differ from the mix by {worst}");
        assert_eq!(Stem::Group(1).file_part(&p), "g2-Group_2");
        assert_eq!(Stem::Track(5).file_part(&p), format!("06-{}", p.tracks[5].name.replace(' ', "_")));
        // Soloing a member leaves the group with just that one.
        p.apply(Edit::SetSolo { track: 1, solo: true }).unwrap();
        assert_eq!(super::stems(&p), [Stem::Group(1)]);
        let held = stem_project(&p, Stem::Group(1));
        assert_eq!(held.tracks.iter().map(|t| t.mute).collect::<Vec<_>>(), [true, false, true, true, true, true]);
        // Only the stem's own group plays: another's inserts could sound
        // with none of its tracks heard.
        assert_eq!(held.groups.iter().map(|g| g.mute).collect::<Vec<_>>(), [true, false, true]);
        let alone = stem_project(&p, Stem::Track(5));
        assert_eq!(alone.groups.iter().map(|g| g.mute).collect::<Vec<_>>(), [true, true, true]);
    }

    #[test]
    fn the_limiter_keeps_exports_on_the_beat_and_under_the_ceiling() {
        use crate::mix::mp;
        let mut p = Project::demo();
        p.mix[mp::LIMITER] = 0.0;
        p.master_gain_db = 0.0;
        // No tail: a louder mix would take longer to fall silent.
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let plain = render(&p, &[], &opts);
        // Below the ceiling the limiter changes nothing, not even timing.
        p.mix[mp::LIMITER] = 1.0;
        p.mix[mp::LIMITER_CEILING] = 0.0;
        let same = render(&p, &[], &opts);
        assert!(
            same == plain,
            "{} of {} samples differ",
            same.iter().zip(&plain).filter(|(a, b)| a != b).count(),
            plain.len()
        );
        // Pushed into it, the file starts at the same frame and stays under.
        p.master_gain_db = 12.0;
        p.mix[mp::LIMITER_CEILING] = -1.0;
        let loud = render(&p, &[], &opts);
        assert_eq!(loud.len(), plain.len());
        let first = |x: &[f32]| x.iter().position(|v| v.abs() > 1e-4);
        assert_eq!(first(&loud), first(&plain));
        let mut tp = [crate::dsp::limiter::TruePeak::new(), crate::dsp::limiter::TruePeak::new()];
        let peak = loud.chunks(2).fold(0.0f32, |m, f| m.max(tp[0].push(f[0])).max(tp[1].push(f[1])));
        assert!(crate::dsp::gain_to_db(peak) <= -0.95, "{}", crate::dsp::gain_to_db(peak));
    }

    #[test]
    fn tail_is_capped() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        // Ten-second release on a note held to the end of the pattern.
        p.tracks[0].params[16] = 10.0;
        p.tracks[0].steps[0][0] = Some(Step::new(48, 127, 16));
        let opts = RenderOptions { loops: 1, max_tail_seconds: 1.0, ..Default::default() };
        let out = render(&p, &[], &opts);
        assert_eq!(out.len() / 2, main_frames(&p, &opts) as usize + 48_000);
    }

    #[test]
    fn an_export_plays_the_clips_and_writes_over_none_of_their_files() {
        let mut p = Project::new();
        p.apply(Edit::InsertSongEntry { index: 0, pattern: 0 }).unwrap();
        p.apply(Edit::AddTrack(InstrumentKind::Audio)).unwrap();
        let dir = std::env::temp_dir().join(format!("tinvo-clip-dest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("take.wav");
        std::fs::write(&file, "x").unwrap();
        let path = file.display().to_string();
        p.tracks[0].clips = vec![crate::clip::ClipData::new(path.clone(), 0, 12, 1000)].into();
        assert_eq!(check_destination(&p, None, &file), Err("is a clip the project plays".into()));
        std::fs::remove_dir_all(&dir).unwrap();
        let mut bank = ClipBank::new();
        bank.insert(path, Arc::new(crate::clip::ClipAudio::new(vec![vec![0.5; 1000]], 48_000).unwrap()));
        let opts = RenderOptions { loops: 1, max_tail_seconds: 0.0, source: Source::Song, ..Default::default() };
        let out = render_with(&p, &[], &bank, &opts);
        // A bar at 120 BPM; the clip from its second beat for 1000 frames.
        assert_eq!(out.len(), 2 * 96_000);
        let first = out.iter().position(|x| x.abs() > 1e-6).map(|i| i / 2);
        assert_eq!(first, Some(12_000));
        assert!(out[2 * 13_000..].iter().all(|&x| x == 0.0));
        // Without its file the clip is silent.
        assert!(render(&p, &[], &opts).iter().all(|&x| x == 0.0));
    }
}
