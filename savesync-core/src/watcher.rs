//! Passive save-directory change detection.
//!
//! This module only observes filesystem activity. It never snapshots, reads
//! save contents, creates directories inside a watched root, or calls into
//! [`crate::engine::SaveSyncEngine`]. Consumers receive a [`DirtyEvent`] when a
//! game's save tree changed and then decide when (or whether) to capture it.
//!
//! [`ChangeTracker`] is a pure state machine over caller-supplied `Instant`s;
//! it performs no I/O and owns no timers. [`SaveWatcher`] wires that state
//! machine to `notify` on a dedicated thread.
//!
//! # Platform caveats
//!
//! `notify` does not honour `follow_symlinks(false)` on Windows. A junction or
//! reparse point inside a registered root can therefore produce events for
//! paths outside that root. Path mapping is purely lexical, so an out-of-tree
//! path is ignored, but there is currently no strict guarantee that activity
//! reached through such a link is invisible. An integration test covers this
//! boundary; tightening it is future work.
//!
//! [`SaveWatcher::stop`] blocks until the worker thread exits. Do not call it
//! from the UI thread; shut the watcher down from a background task instead.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::EventKind;
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::error::{Error, Result};
use crate::paths::validate_root;

/// How long a save tree must stay untouched, and how long it may keep changing,
/// before a dirty event is emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebouncePolicy {
    /// Emit once a game has been quiet for this long.
    pub quiet: Duration,
    /// Emit even while changes continue after this long.
    pub max_hold: Duration,
}

impl Default for DebouncePolicy {
    fn default() -> Self {
        Self {
            quiet: Duration::from_secs(5),
            max_hold: Duration::from_secs(60),
        }
    }
}

/// Why a dirty event was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DirtyReason {
    /// The save tree stopped changing for [`DebouncePolicy::quiet`].
    Quiet,
    /// The save tree kept changing for [`DebouncePolicy::max_hold`].
    MaxHold,
}

/// A game whose save tree changed.
///
/// `generation` identifies the dirty period this event belongs to. It
/// increments every time a successful event is produced for the game. Pass it
/// back to [`ChangeTracker::clear`] or [`SaveWatcher::clear`] so a delayed
/// acknowledgement cannot discard newer dirty state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DirtyEvent {
    pub game_id: String,
    pub reason: DirtyReason,
    pub generation: u64,
}

#[derive(Debug)]
struct PendingGame {
    first_seen: Instant,
    last_change: Instant,
    /// A `DirtyEvent` has been produced for this dirty period and the consumer
    /// has not called [`ChangeTracker::clear`] yet.
    reported: bool,
    /// Generation of the most recently reported dirty period. The next
    /// successful report uses `generation + 1`.
    generation: u64,
}

/// Debounce state for a set of games.
///
/// The tracker is pure: callers supply `Instant`s. It coalesces any number of
/// observations for a game into at most one pending entry.
///
/// [`Self::observe`] only records activity; [`Self::poll`] decides when a game
/// becomes dirty. A single change followed by silence is therefore reported by
/// a later `poll` even when no further filesystem event arrives.
///
/// A reported game stays pending until [`Self::clear`] is called. New activity
/// on a reported game re-arms it, so it can be reported again after a fresh
/// quiet period.
#[derive(Debug)]
pub struct ChangeTracker {
    policy: DebouncePolicy,
    games: HashMap<String, PendingGame>,
}

impl ChangeTracker {
    pub fn new(policy: DebouncePolicy) -> Self {
        Self {
            policy,
            games: HashMap::new(),
        }
    }

    /// Record filesystem activity for `game_id`.
    ///
    /// The first observation inserts the game with `first_seen = last_change =
    /// now` and `reported = false`. Later observations update `last_change`;
    /// if the game was already reported, they re-arm it with `reported =
    /// false` and `first_seen = now`.
    ///
    /// Emission happens exclusively in [`Self::poll`] followed by
    /// [`Self::mark_reported`].
    pub fn observe(&mut self, game_id: &str, now: Instant) {
        if let Some(game) = self.games.get_mut(game_id) {
            game.last_change = now;
            if game.reported {
                game.reported = false;
                game.first_seen = now;
            }
        } else {
            self.games.insert(
                game_id.to_owned(),
                PendingGame {
                    first_seen: now,
                    last_change: now,
                    reported: false,
                    generation: 0,
                },
            );
        }
    }

    /// Return dirty events for games whose debounce deadline has passed.
    ///
    /// For every game that has not been reported yet:
    ///
    /// - [`DirtyReason::MaxHold`] when `now - first_seen >= max_hold`;
    /// - otherwise [`DirtyReason::Quiet`] when
    ///   `now - last_change >= quiet`.
    ///
    /// This method does not mutate the tracker. It is safe to call it, fail to
    /// deliver some or all of the returned events (for example because a
    /// bounded output channel is full), and call it again: the same candidates
    /// are returned until [`Self::mark_reported`] is called for each event that
    /// was actually delivered.
    ///
    /// Events are sorted by `game_id` so callers observe a deterministic order.
    pub fn poll(&self, now: Instant) -> Vec<DirtyEvent> {
        let mut due: Vec<(String, DirtyReason, u64)> = self
            .games
            .iter()
            .filter(|(_, game)| !game.reported)
            .filter_map(|(game_id, game)| {
                let reason =
                    if now.saturating_duration_since(game.first_seen) >= self.policy.max_hold {
                        DirtyReason::MaxHold
                    } else if now.saturating_duration_since(game.last_change) >= self.policy.quiet {
                        DirtyReason::Quiet
                    } else {
                        return None;
                    };
                Some((game_id.clone(), reason, game.generation.saturating_add(1)))
            })
            .collect();
        due.sort_by(|left, right| left.0.cmp(&right.0));

        due.into_iter()
            .map(|(game_id, reason, generation)| DirtyEvent {
                game_id,
                reason,
                generation,
            })
            .collect()
    }

    /// Record that the event for `generation` was actually delivered.
    ///
    /// Call this only after the corresponding [`Self::poll`] candidate was
    /// accepted by the consumer. Until then the game stays unreported and the
    /// same candidate is returned by later polls.
    pub fn mark_reported(&mut self, game_id: &str, generation: u64) {
        let Some(game) = self.games.get_mut(game_id) else {
            return;
        };
        if game.reported || game.generation.saturating_add(1) != generation {
            return;
        }
        game.generation = generation;
        game.reported = true;
    }

    /// Games with dirty state that has not been acknowledged with
    /// [`Self::clear`], sorted by game id.
    ///
    /// This includes games whose event was already emitted but not yet cleared.
    pub fn pending(&self) -> Vec<&str> {
        let mut games: Vec<&str> = self.games.keys().map(String::as_str).collect();
        games.sort_unstable();
        games
    }

    /// Acknowledge a previously reported dirty period.
    ///
    /// The state is removed only when `generation` matches the generation that
    /// was actually reported and the game has not been re-armed by new
    /// activity. A delayed acknowledgement for an older generation is ignored,
    /// so dirty state produced by newer changes survives.
    pub fn clear(&mut self, game_id: &str, generation: u64) {
        let matches = self
            .games
            .get(game_id)
            .is_some_and(|game| game.reported && game.generation == generation);
        if matches {
            self.games.remove(game_id);
        }
    }

    /// Remove all dirty state for a game regardless of generation.
    ///
    /// Used when the game is no longer watched; there is no consumer event left
    /// to acknowledge.
    fn discard(&mut self, game_id: &str) {
        self.games.remove(game_id);
    }
}

#[derive(Debug, Clone)]
struct RootEntry {
    game_id: String,
    root: PathBuf,
}

/// Return the game whose registered root is the longest prefix of `path`.
///
/// Matching is component-wise and purely lexical: a registration for `games`
/// never matches `games2`. Nested roots are resolved in favour of the most
/// specific (longest) root.
fn game_for_path<'a>(path: &Path, roots: &'a [RootEntry]) -> Option<&'a str> {
    roots
        .iter()
        .filter(|entry| path.starts_with(&entry.root))
        .max_by_key(|entry| entry.root.as_os_str().len())
        .map(|entry| entry.game_id.as_str())
}

/// Temporary files written by editors, launchers, and save tools.
///
/// These usually appear and disappear around the real save write. Treating
/// them as changes would produce dirty events for work that is already gone.
///
/// Suffixes are compared case-insensitively on every platform. Windows paths
/// are case-insensitive, and save trees may be shared with case-insensitive
/// filesystems, so filtering the superset avoids platform-dependent behaviour.
fn is_temporary_artifact_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".part")
        || name.ends_with(".tmp")
        || name.ends_with(".lock")
        || name.ends_with('~')
        || name.starts_with("~$")
}

fn is_temporary_artifact_path(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| is_temporary_artifact_name(&name.to_string_lossy()))
}

/// Match SaveSync's own working directories case-insensitively.
///
/// The canonical check lives in [`crate::paths::is_internal_artifact`]; the
/// lowercase retry covers case-insensitive filesystems and mixed-case paths
/// reported by the OS.
fn is_internal_artifact_component(name: &str) -> bool {
    crate::paths::is_internal_artifact(name)
        || crate::paths::is_internal_artifact(&name.to_ascii_lowercase())
}

/// Return true for paths that must never make a game dirty.
///
/// Internal artifacts are checked in every path component, not only the final
/// name: a change below `.savesync/` or `.savesync-tmp-*` is still an
/// implementation detail of SaveSync itself.
fn is_ignored_path(path: &Path) -> bool {
    let internal = path.components().any(|component| match component {
        Component::Normal(name) => is_internal_artifact_component(&name.to_string_lossy()),
        _ => false,
    });
    internal || is_temporary_artifact_path(path)
}

/// Interval used to flush max-hold deadlines even when no filesystem events
/// arrive.
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum number of dirty events buffered for the consumer before new events
/// are dropped and counted.
const EVENT_QUEUE_CAPACITY: usize = 64;

/// Maximum number of filesystem and control messages buffered for the worker.
///
/// When the platform callback cannot enqueue an event, the worker is told that
/// activity was lost and re-observes every registered root instead of growing
/// memory without bound.
const COMMAND_QUEUE_CAPACITY: usize = 1024;

enum Command {
    Watch {
        game_id: String,
        root: PathBuf,
        reply: SyncSender<Result<()>>,
    },
    Unwatch {
        game_id: String,
        reply: SyncSender<Result<()>>,
    },
    Clear {
        game_id: String,
        generation: u64,
        reply: SyncSender<Result<()>>,
    },
    Pending {
        reply: SyncSender<Vec<String>>,
    },
    Stop,
    Fs(notify::Result<Event>),
}

/// A running filesystem watcher with one shared worker thread.
///
/// Events are coalesced per game by [`ChangeTracker`] and delivered over a
/// bounded channel ([`EVENT_QUEUE_CAPACITY`]). When that channel is full the
/// worker drops the event for that poll and counts it, rather than blocking
/// the watcher thread; see [`Self::dropped_events`]. A dropped event is not
/// marked reported, so the same candidate is retried on the next flush.
///
/// A dropped event does not lose the tracker's internal per-game state: the
/// game remains pending inside the worker and can be recovered with
/// [`Self::pending_games`], so after reconnecting the consumer can reconcile
/// without relying on events. Dropping the receiver never blocks the worker.
pub struct SaveWatcher {
    commands: SyncSender<Command>,
    stopped: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}

impl SaveWatcher {
    /// Start a watcher with [`DebouncePolicy::default`].
    pub fn start() -> Result<(Self, Receiver<DirtyEvent>)> {
        Self::start_with_policy(DebouncePolicy::default())
    }

    /// Start a watcher with an explicit debounce policy.
    ///
    /// `quiet` must be greater than zero and `max_hold` must not be shorter
    /// than `quiet`; otherwise a game could be reported before it ever had a
    /// chance to become quiet.
    pub fn start_with_policy(policy: DebouncePolicy) -> Result<(Self, Receiver<DirtyEvent>)> {
        if policy.quiet.is_zero() {
            return Err(Error::InvalidState(
                "debounce policy requires quiet > 0; got 0 ns".into(),
            ));
        }
        if policy.max_hold < policy.quiet {
            return Err(Error::InvalidState(format!(
                "debounce policy requires max_hold >= quiet; got max_hold={:?}, quiet={:?}",
                policy.max_hold, policy.quiet
            )));
        }

        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let callback_tx = command_tx.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);

        let join = thread::Builder::new()
            .name("savesync-save-watcher".into())
            .spawn(move || {
                run_worker(
                    command_rx,
                    callback_tx,
                    event_tx,
                    policy,
                    ready_tx,
                    worker_dropped,
                    worker_stopped,
                );
            })
            .map_err(|error| Error::io("<save watcher thread>", error))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok((
                Self {
                    commands: command_tx,
                    stopped,
                    join: Some(join),
                    dropped,
                },
                event_rx,
            )),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => {
                let _ = join.join();
                Err(Error::InvalidState(
                    "save watcher thread stopped during startup".into(),
                ))
            }
        }
    }

    /// Watch `root` recursively for changes belonging to `game_id`.
    ///
    /// The root must already exist and is canonicalized before use. The same
    /// game id or the same canonical root may only be registered once.
    pub fn watch(&mut self, game_id: &str, root: &Path) -> Result<()> {
        let root = validate_root(root)?;
        self.request(|reply| Command::Watch {
            game_id: game_id.to_owned(),
            root,
            reply,
        })
    }

    /// Stop watching the root registered for `game_id` and discard any
    /// unreported dirty state for it.
    pub fn unwatch(&mut self, game_id: &str) -> Result<()> {
        self.request(|reply| Command::Unwatch {
            game_id: game_id.to_owned(),
            reply,
        })
    }

    /// Acknowledge that the consumer handled the dirty state for `game_id`.
    ///
    /// `generation` must be the value from the [`DirtyEvent`] that was handled.
    /// If the game has produced a newer dirty period in the meantime, the
    /// acknowledgement is ignored instead of discarding that newer state.
    ///
    /// Until this is called, a game that already produced a `DirtyEvent` is not
    /// reported again unless new activity re-arms it.
    pub fn clear(&mut self, game_id: &str, generation: u64) -> Result<()> {
        self.request(|reply| Command::Clear {
            game_id: game_id.to_owned(),
            generation,
            reply,
        })
    }

    /// Games with dirty state that has not been cleared yet, sorted by id.
    ///
    /// This queries the worker directly, so a consumer that reconnects can
    /// reconcile without relying on previously delivered events. Returns an
    /// empty list if the watcher is no longer running.
    pub fn pending_games(&self) -> Vec<String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Pending { reply: reply_tx })
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.recv().unwrap_or_default()
    }

    /// Number of dirty events dropped because the consumer channel was full.
    ///
    /// The counter is monotonic for the lifetime of this watcher. A dropped
    /// event does not discard the worker's debounce state, so the affected
    /// game remains pending and can be handled by the consumer after
    /// reconnecting, for example by calling [`Self::clear`] for games it has
    /// already reconciled.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop watching, drop the platform watcher, and join the worker thread.
    ///
    /// This call blocks until the worker exits. Do not call it from the UI
    /// thread; use a background task instead.
    pub fn stop(mut self) -> Result<()> {
        self.shutdown()
    }

    fn request(&self, command: impl FnOnce(SyncSender<Result<()>>) -> Command) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.commands
            .send(command(reply_tx))
            .map_err(|_| Error::InvalidState("save watcher is not running".into()))?;
        reply_rx
            .recv()
            .map_err(|_| Error::InvalidState("save watcher stopped before replying".into()))?
    }

    fn shutdown(&mut self) -> Result<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        // The flag is checked before every loop iteration, so the worker does
        // not have to drain queued filesystem events before exiting. `try_send`
        // keeps shutdown non-blocking even when the command queue is full.
        self.stopped.store(true, Ordering::Release);
        let _ = self.commands.try_send(Command::Stop);
        join.join()
            .map_err(|_| Error::InvalidState("save watcher thread panicked".into()))
    }
}

impl Drop for SaveWatcher {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn run_worker(
    commands: Receiver<Command>,
    callback_tx: SyncSender<Command>,
    events: SyncSender<DirtyEvent>,
    policy: DebouncePolicy,
    ready: SyncSender<Result<()>>,
    dropped: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
) {
    // If the bounded command queue overflows, enqueueing from the platform
    // callback would block the backend. Instead, mark that activity was lost;
    // the worker re-observes every registered root on its next iteration.
    let activity_lost = Arc::new(AtomicBool::new(false));
    let callback_lost = Arc::clone(&activity_lost);

    // Filesystem events share the control queue so the worker has a single
    // ordered wait point and cannot starve either source.
    let watcher = RecommendedWatcher::new(
        move |event| {
            // A dropped consumer must never block the platform watcher.
            match callback_tx.try_send(Command::Fs(event)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    callback_lost.store(true, Ordering::Release);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        },
        NotifyConfig::default().with_follow_symlinks(false),
    );

    let mut watcher = match watcher {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = ready.send(Err(Error::InvalidState(format!(
                "failed to create save watcher: {error}"
            ))));
            return;
        }
    };

    if ready.send(Ok(())).is_err() {
        return;
    }

    let mut tracker = ChangeTracker::new(policy);
    let mut roots: Vec<RootEntry> = Vec::new();

    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }

        if activity_lost.swap(false, Ordering::AcqRel) {
            // One observation per game is enough; the debounce policy
            // coalesces any number of them.
            let now = Instant::now();
            for entry in &roots {
                tracker.observe(&entry.game_id, now);
            }
        }

        let message = if roots.is_empty() {
            match commands.recv() {
                Ok(message) => Some(message),
                Err(_) => break,
            }
        } else {
            match commands.recv_timeout(FLUSH_INTERVAL) {
                Ok(message) => Some(message),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };

        match message {
            Some(Command::Fs(event)) => {
                // Record changes now; the flush below emits both quiet and
                // max-hold deadlines reached by this event.
                handle_event(event, &roots, &mut tracker);
            }
            Some(Command::Watch {
                game_id,
                root,
                reply,
            }) => {
                let result = add_root(&mut roots, &mut watcher, game_id, root);
                let _ = reply.send(result);
            }
            Some(Command::Unwatch { game_id, reply }) => {
                let result = remove_root(&mut roots, &mut watcher, &mut tracker, &game_id);
                let _ = reply.send(result);
            }
            Some(Command::Clear {
                game_id,
                generation,
                reply,
            }) => {
                tracker.clear(&game_id, generation);
                let _ = reply.send(Ok(()));
            }
            Some(Command::Pending { reply }) => {
                let games = tracker.pending().into_iter().map(str::to_owned).collect();
                let _ = reply.send(games);
            }
            Some(Command::Stop) => break,
            None => {}
        }

        flush_due(&mut tracker, &events, &dropped, Instant::now());
    }

    // Dropping the watcher unregisters all paths and stops the backend.
    drop(watcher);
}

fn add_root(
    roots: &mut Vec<RootEntry>,
    watcher: &mut RecommendedWatcher,
    game_id: String,
    root: PathBuf,
) -> Result<()> {
    if let Some(existing) = roots.iter().find(|entry| entry.game_id == game_id) {
        return Err(Error::InvalidState(format!(
            "save location is already watched for game {game_id}: {}",
            existing.root.display()
        )));
    }
    if let Some(existing) = roots.iter().find(|entry| entry.root == root) {
        return Err(Error::InvalidState(format!(
            "save location {} is already watched for game {}",
            root.display(),
            existing.game_id
        )));
    }

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|error| watcher_error(&root, error))?;
    roots.push(RootEntry { game_id, root });
    Ok(())
}

fn remove_root(
    roots: &mut Vec<RootEntry>,
    watcher: &mut RecommendedWatcher,
    tracker: &mut ChangeTracker,
    game_id: &str,
) -> Result<()> {
    let index = roots
        .iter()
        .position(|entry| entry.game_id == game_id)
        .ok_or_else(|| Error::NotFound(format!("no watched save location for game {game_id}")))?;

    let root = roots[index].root.clone();
    match watcher.unwatch(&root) {
        Ok(()) => {}
        Err(error) if matches!(&error.kind, notify::ErrorKind::WatchNotFound) => {
            // The backend already forgot this watch. Remove the local entry
            // anyway so the game does not stay in a zombie state.
        }
        Err(error) => return Err(watcher_error(&root, error)),
    }
    roots.remove(index);
    tracker.discard(game_id);
    Ok(())
}

fn handle_event(event: notify::Result<Event>, roots: &[RootEntry], tracker: &mut ChangeTracker) {
    let Ok(event) = event else {
        // Backend errors are advisory here; there is no safe local mutation to
        // roll back. A future watch remains usable.
        return;
    };
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }

    let now = Instant::now();
    for path in &event.paths {
        if is_ignored_path(path) {
            continue;
        }
        let Some(game_id) = game_for_path(path, roots) else {
            continue;
        };
        tracker.observe(game_id, now);
    }
}

/// Emit every due event, dropping (and counting) events when the consumer
/// channel is full so the worker never blocks on an unresponsive consumer.
///
/// A candidate is marked reported only after `try_send` succeeds. If the
/// channel is full, the same generation remains pending and is retried on the
/// next flush.
fn flush_due(
    tracker: &mut ChangeTracker,
    events: &SyncSender<DirtyEvent>,
    dropped: &AtomicU64,
    now: Instant,
) {
    for dirty in tracker.poll(now) {
        if send_dirty(events, dropped, &dirty) {
            tracker.mark_reported(&dirty.game_id, dirty.generation);
        }
    }
}

/// Return `true` only when the event was accepted by the consumer.
fn send_dirty(events: &SyncSender<DirtyEvent>, dropped: &AtomicU64, dirty: &DirtyEvent) -> bool {
    match events.try_send(dirty.clone()) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            // The consumer is gone; there is nobody left to receive events.
            false
        }
    }
}

fn watcher_error(path: &Path, error: notify::Error) -> Error {
    match error.kind {
        notify::ErrorKind::Io(source) => Error::io(path, source),
        notify::ErrorKind::PathNotFound => Error::io(
            path,
            io::Error::new(io::ErrorKind::NotFound, "watched path does not exist"),
        ),
        notify::ErrorKind::WatchNotFound => Error::io(
            path,
            io::Error::new(io::ErrorKind::NotFound, "watch does not exist"),
        ),
        notify::ErrorKind::MaxFilesWatch => {
            Error::io(path, io::Error::other("OS file watch limit reached"))
        }
        notify::ErrorKind::InvalidConfig(config) => Error::io(
            path,
            io::Error::other(format!("invalid watcher config: {config:?}")),
        ),
        notify::ErrorKind::Generic(message) => Error::io(path, io::Error::other(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn policy(quiet_secs: u64, max_hold_secs: u64) -> DebouncePolicy {
        DebouncePolicy {
            quiet: Duration::from_secs(quiet_secs),
            max_hold: Duration::from_secs(max_hold_secs),
        }
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    fn root(game_id: &str, path: &str) -> RootEntry {
        RootEntry {
            game_id: game_id.to_owned(),
            root: PathBuf::from(path),
        }
    }

    fn event_at(game_id: &str, reason: DirtyReason, generation: u64) -> DirtyEvent {
        DirtyEvent {
            game_id: game_id.to_owned(),
            reason,
            generation,
        }
    }

    fn event(game_id: &str, reason: DirtyReason) -> DirtyEvent {
        event_at(game_id, reason, 1)
    }

    /// Poll and acknowledge everything, mirroring a worker whose `try_send`
    /// succeeded for every candidate.
    fn report(tracker: &mut ChangeTracker, now: Instant) -> Vec<DirtyEvent> {
        let events = tracker.poll(now);
        for dirty in &events {
            tracker.mark_reported(&dirty.game_id, dirty.generation);
        }
        events
    }

    #[test]
    fn first_observation_only_marks_the_game_pending() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        assert_eq!(tracker.pending(), vec!["game-a"]);
        assert!(tracker.poll(at(base, 4)).is_empty());
        assert_eq!(tracker.pending(), vec!["game-a"]);
    }

    #[test]
    fn single_change_followed_by_silence_reports_quiet() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        // No further filesystem event arrives; the periodic poll must still
        // observe the quiet deadline.
        let events = tracker.poll(at(base, 5));
        assert_eq!(events, vec![event("game-a", DirtyReason::Quiet)]);
    }

    #[test]
    fn poll_before_the_quiet_deadline_emits_nothing() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        assert!(tracker.poll(base).is_empty());
        assert!(tracker.poll(base + Duration::from_millis(4_999)).is_empty());
    }

    #[test]
    fn new_event_restarts_the_quiet_period() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        tracker.observe("game-a", at(base, 4));
        // 8 seconds after the first observation but only 4 after the latest.
        assert!(tracker.poll(at(base, 8)).is_empty());
        let events = tracker.poll(at(base, 9));
        assert_eq!(events, vec![event("game-a", DirtyReason::Quiet)]);
    }

    #[test]
    fn max_hold_fires_during_continuous_activity_and_keeps_state_until_clear() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 20));

        tracker.observe("game-a", base);
        for secs in [2, 5, 8, 11, 14, 17] {
            tracker.observe("game-a", at(base, secs));
        }
        assert!(tracker.poll(at(base, 19)).is_empty());

        let events = report(&mut tracker, at(base, 20));
        assert_eq!(events, vec![event("game-a", DirtyReason::MaxHold)]);
        // Reported state stays pending until the consumer acknowledges it.
        assert_eq!(tracker.pending(), vec!["game-a"]);
        assert!(tracker.poll(at(base, 21)).is_empty());

        tracker.clear("game-a", 1);
        assert!(tracker.pending().is_empty());
        assert!(tracker.poll(at(base, 21)).is_empty());
    }

    #[test]
    fn max_hold_takes_priority_when_both_deadlines_are_due() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 10));

        tracker.observe("game-a", base);
        tracker.observe("game-a", at(base, 5));
        // Quiet is due (5s since the latest change) and max-hold is due too.
        let events = tracker.poll(at(base, 10));
        assert_eq!(events, vec![event("game-a", DirtyReason::MaxHold)]);
    }

    #[test]
    fn games_are_tracked_independently() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        tracker.observe("game-b", at(base, 3));

        assert_eq!(
            report(&mut tracker, at(base, 5)),
            vec![event("game-a", DirtyReason::Quiet)]
        );
        assert_eq!(
            report(&mut tracker, at(base, 8)),
            vec![event("game-b", DirtyReason::Quiet)]
        );
    }

    #[test]
    fn poll_reports_games_sorted_by_game_id() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        for game_id in ["game-c", "game-a", "game-b"] {
            tracker.observe(game_id, base);
        }

        let events = tracker.poll(at(base, 5));
        let ids: Vec<&str> = events.iter().map(|dirty| dirty.game_id.as_str()).collect();
        assert_eq!(ids, vec!["game-a", "game-b", "game-c"]);
        assert!(
            events
                .iter()
                .all(|dirty| dirty.reason == DirtyReason::Quiet)
        );
    }

    #[test]
    fn quiet_is_not_repeated_until_clear() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        assert_eq!(
            report(&mut tracker, at(base, 5)),
            vec![event("game-a", DirtyReason::Quiet)]
        );

        // The deadline keeps passing, but the report is already outstanding.
        assert!(tracker.poll(at(base, 30)).is_empty());
        assert!(tracker.poll(at(base, 59)).is_empty());
        assert_eq!(tracker.pending(), vec!["game-a"]);

        tracker.clear("game-a", 1);
        assert!(tracker.pending().is_empty());
        assert!(tracker.poll(at(base, 59)).is_empty());
    }

    #[test]
    fn new_activity_after_quiet_rearms_and_reports_again() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        assert_eq!(
            report(&mut tracker, at(base, 5)),
            vec![event("game-a", DirtyReason::Quiet)]
        );

        // Activity after the report re-arms the game with a fresh deadline.
        tracker.observe("game-a", at(base, 6));
        assert_eq!(tracker.pending(), vec!["game-a"]);
        assert!(tracker.poll(at(base, 10)).is_empty());
        assert_eq!(
            report(&mut tracker, at(base, 11)),
            vec![event_at("game-a", DirtyReason::Quiet, 2)]
        );
        // Reported again: no repeat until further activity or clear.
        assert!(tracker.poll(at(base, 40)).is_empty());
    }

    #[test]
    fn full_queue_drops_and_counts_events_without_blocking() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(1, 60));
        tracker.observe("game-a", base);

        let (events, receiver) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(0);

        // Fill the queue with an unrelated event.
        assert!(send_dirty(
            &events,
            &dropped,
            &event("filler", DirtyReason::Quiet)
        ));
        // The due candidate for game-a cannot be delivered.
        flush_due(&mut tracker, &events, &dropped, at(base, 1));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);

        // `try_send` failing must not mark the game reported: the same
        // generation is still a candidate on the next poll.
        assert_eq!(
            tracker.poll(at(base, 2)),
            vec![event("game-a", DirtyReason::Quiet)]
        );
        assert_eq!(tracker.pending(), vec!["game-a"]);

        // Drain the filler and retry on the next flush.
        assert_eq!(receiver.recv().unwrap().game_id, "filler");
        flush_due(&mut tracker, &events, &dropped, at(base, 2));
        assert_eq!(
            receiver.recv().unwrap(),
            event("game-a", DirtyReason::Quiet)
        );
        assert!(receiver.try_recv().is_err());
        // Only the successful send marks the state as reported.
        assert!(tracker.poll(at(base, 3)).is_empty());
        assert_eq!(tracker.pending(), vec!["game-a"]);
        tracker.clear("game-a", 1);
        assert!(tracker.pending().is_empty());
    }

    #[test]
    fn dropped_receiver_neither_blocks_nor_counts_as_backpressure() {
        let (events, receiver) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(0);
        drop(receiver);

        assert!(!send_dirty(
            &events,
            &dropped,
            &event("game-a", DirtyReason::Quiet)
        ));

        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn temporary_artifact_names_are_filtered() {
        for accepted in [
            "save.sav",
            "slot1.dat",
            "data.bin",
            "~profile",
            "file.tmp.bak",
        ] {
            assert!(
                !is_temporary_artifact_name(accepted),
                "expected {accepted:?} to be accepted"
            );
        }
        for rejected in [
            "save.sav.part",
            "save.tmp",
            "session.lock",
            "save~",
            "~$document.docx",
        ] {
            assert!(
                is_temporary_artifact_name(rejected),
                "expected {rejected:?} to be filtered"
            );
        }
    }

    #[test]
    fn path_mapping_uses_the_longest_root_and_rejects_outsiders() {
        let roots = vec![
            root("outer", "games"),
            root("inner", "games/slot1"),
            root("sibling", "library"),
        ];

        assert_eq!(
            game_for_path(Path::new("games/slot1/save.dat"), &roots),
            Some("inner")
        );
        assert_eq!(
            game_for_path(Path::new("games/slot2/save.dat"), &roots),
            Some("outer")
        );
        // A root itself belongs to its most specific registration.
        assert_eq!(
            game_for_path(Path::new("games/slot1"), &roots),
            Some("inner")
        );
        assert_eq!(
            game_for_path(Path::new("library/save.dat"), &roots),
            Some("sibling")
        );
        assert_eq!(game_for_path(Path::new("elsewhere/save.dat"), &roots), None);
        // Component-wise matching: "games2" is not inside "games".
        assert_eq!(game_for_path(Path::new("games2/save.dat"), &roots), None);
    }

    #[test]
    fn stale_generation_clear_keeps_rearmed_state() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        assert_eq!(
            report(&mut tracker, at(base, 5)),
            vec![event("game-a", DirtyReason::Quiet)]
        );

        // New activity re-arms the game before the consumer acknowledges the
        // first event. The old generation must not clear this state.
        tracker.observe("game-a", at(base, 6));
        tracker.clear("game-a", 1);
        assert_eq!(tracker.pending(), vec!["game-a"]);

        let rearmed = report(&mut tracker, at(base, 11));
        assert_eq!(rearmed, vec![event_at("game-a", DirtyReason::Quiet, 2)]);
        // A delayed acknowledgement for generation 1 is still stale.
        tracker.clear("game-a", 1);
        assert_eq!(tracker.pending(), vec!["game-a"]);
    }

    #[test]
    fn matching_generation_clear_removes_state() {
        let base = Instant::now();
        let mut tracker = ChangeTracker::new(policy(5, 60));

        tracker.observe("game-a", base);
        assert_eq!(
            report(&mut tracker, at(base, 5)),
            vec![event("game-a", DirtyReason::Quiet)]
        );

        tracker.clear("game-a", 1);
        assert!(tracker.pending().is_empty());
        assert!(tracker.poll(at(base, 30)).is_empty());
    }

    #[test]
    fn internal_artifact_components_and_uppercase_suffixes_are_filtered() {
        for filtered in [
            ".SAVESYNC/save.dat",
            ".savesync-tmp-1234/slot/save.dat",
            "slot/.savesync/tmp.bin",
            "slot/.savesync-tmp-abc/working.bin",
            "slot/save.TMP",
            "slot/SAVE.PART",
            "slot/session.LOCK",
        ] {
            assert!(
                is_ignored_path(Path::new(filtered)),
                "expected {filtered:?} to be filtered"
            );
        }

        for accepted in [
            "slot/save.sav",
            "SAVESYNC/save.dat",
            ".savesync-backup/save.dat",
        ] {
            assert!(
                !is_ignored_path(Path::new(accepted)),
                "expected {accepted:?} to be accepted"
            );
        }
    }

    #[test]
    fn start_with_policy_rejects_invalid_debounce_windows() {
        let error = SaveWatcher::start_with_policy(DebouncePolicy {
            quiet: Duration::from_secs(10),
            max_hold: Duration::from_secs(9),
        })
        .err()
        .expect("max_hold < quiet must be rejected");
        assert!(
            matches!(&error, Error::InvalidState(message) if message.contains("max_hold")),
            "unexpected error: {error:?}"
        );

        let error = SaveWatcher::start_with_policy(DebouncePolicy {
            quiet: Duration::ZERO,
            max_hold: Duration::from_secs(1),
        })
        .err()
        .expect("zero quiet must be rejected");
        assert!(
            matches!(&error, Error::InvalidState(message) if message.contains("quiet")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn real_watcher_reports_dirty_event_for_registered_root() {
        let dir = tempfile::tempdir().unwrap();
        let (mut watcher, receiver) = SaveWatcher::start_with_policy(DebouncePolicy {
            quiet: Duration::from_millis(50),
            max_hold: Duration::from_secs(5),
        })
        .unwrap();

        watcher.watch("game-integration", dir.path()).unwrap();
        fs::write(dir.path().join("save.dat"), b"save-data").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let dirty = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for a DirtyEvent for game-integration"
            );
            match receiver.recv_timeout(remaining) {
                Ok(dirty) if dirty.game_id == "game-integration" => break dirty,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for a DirtyEvent for game-integration")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("watcher event channel closed unexpectedly")
                }
            }
        };

        assert!(dirty.generation >= 1);
        assert_eq!(
            watcher.pending_games(),
            vec!["game-integration".to_string()]
        );
        watcher.clear("game-integration", dirty.generation).unwrap();
        assert!(watcher.pending_games().is_empty());
        watcher.stop().unwrap();
    }

    #[test]
    fn real_watcher_ignores_internal_tmp_subdirectory_events() {
        let dir = tempfile::tempdir().unwrap();
        let internal = dir
            .path()
            .join(format!(".savesync-tmp-{}", uuid::Uuid::new_v4()));
        // Create the internal directory before watching so its creation is not
        // attributed to the game; only the write below must be ignored.
        fs::create_dir(&internal).unwrap();

        let (mut watcher, receiver) = SaveWatcher::start_with_policy(DebouncePolicy {
            quiet: Duration::from_millis(50),
            max_hold: Duration::from_secs(5),
        })
        .unwrap();
        watcher.watch("game-integration", dir.path()).unwrap();
        fs::write(internal.join("working.bin"), b"partial").unwrap();

        // Negative assertion: no game event may arrive during a short, explicit
        // observation window.
        match receiver.recv_timeout(Duration::from_millis(1_500)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(event) => panic!("unexpected event for internal artifact: {event:?}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("watcher event channel closed unexpectedly")
            }
        }
        watcher.stop().unwrap();
    }
}
