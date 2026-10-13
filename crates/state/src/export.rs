use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{Context, Entity, Task};
use music::{MediaKind, MusicApi, Playlist, Track};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc::UnboundedSender;

use crate::{Io, Outcome, Session, SessionEvent, Toasts, join};

/// The file holding the whole library, beside the spreadsheets.
const ARCHIVE: &str = "library.json";
/// The columns of every spreadsheet, in order. They are a schema rather than copy, so they stay
/// the same in every language and match the keys of `library.json`.
const COLUMNS: [&str; 13] = [
    "position",
    "title",
    "artists",
    "album",
    "duration_ms",
    "isrc",
    "added_at",
    "added_by",
    "explicit",
    "popularity",
    "url",
    "id",
    "album_id",
];
/// Lets a spreadsheet app read the file as UTF-8, which Excel otherwise guesses wrong.
const BOM: &str = "\u{feff}";
/// How long a file name made from a playlist name gets, in characters.
const NAME_LIMIT: usize = 120;
/// How many numbered folders are tried before the export gives up on finding a free name.
const ATTEMPTS: usize = 100;
/// Names Windows keeps for devices, which no file may take whatever its extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Where an export of the streaming library stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ExportState {
    #[default]
    Idle,
    /// Reading from the provider: `done` of `total` lists, the liked songs counting as one.
    /// `total` is zero until the playlists have been listed.
    Reading {
        done: usize,
        total: usize,
    },
    Writing,
    /// The files are in `folder`. `skipped` counts the playlists the provider would not hand
    /// over, which the export left out rather than failing.
    Done {
        folder: PathBuf,
        lists: usize,
        tracks: usize,
        skipped: usize,
    },
    Failed(String),
}

/// What the export calls the things it writes, resolved by the caller in the listener's
/// language. `folder` is followed by the date.
#[derive(Clone, Debug)]
pub struct ExportNames {
    pub folder: String,
    pub liked: String,
    pub playlists: String,
}

/// Writes the streaming library's liked songs and playlists to a folder, as one spreadsheet
/// per list and one `library.json` holding everything. Only metadata is written, never audio.
pub struct Export {
    session: Entity<Session>,
    io: Io,
    state: ExportState,
    task: Option<Task<()>>,
}

#[derive(Serialize)]
struct Archive {
    exported_at: String,
    provider: &'static str,
    liked_songs: Vec<Row>,
    playlists: Vec<List>,
}

#[derive(Serialize)]
struct List {
    id: String,
    name: String,
    owner: String,
    url: Option<String>,
    collaborative: bool,
    public: bool,
    tracks: Vec<Row>,
}

#[derive(Serialize)]
struct Row {
    position: usize,
    title: String,
    artists: Vec<String>,
    album: String,
    duration_ms: u64,
    isrc: Option<String>,
    added_at: Option<String>,
    added_by: Option<String>,
    explicit: bool,
    popularity: u32,
    url: Option<String>,
    id: Option<String>,
    album_id: Option<String>,
}

/// What a finished export reports.
struct Summary {
    folder: PathBuf,
    lists: usize,
    tracks: usize,
    skipped: usize,
}

impl Export {
    pub(crate) fn new(session: Entity<Session>, io: Io, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&session, |this, _, event, cx| {
            if matches!(event, SessionEvent::SignedOut) {
                this.task = None;
                this.state = ExportState::Idle;
                cx.notify();
            }
        })
        .detach();

        Self {
            session,
            io,
            state: ExportState::Idle,
            task: None,
        }
    }

    pub fn state(&self) -> &ExportState {
        &self.state
    }

    pub fn running(&self) -> bool {
        matches!(
            self.state,
            ExportState::Reading { .. } | ExportState::Writing
        )
    }

    /// Exports into a new folder inside `parent`, then shows it in the file manager. Does
    /// nothing while an export is already running or no streaming account is signed in.
    pub fn start(&mut self, parent: PathBuf, names: ExportNames, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let session = self.session.read(cx);
        let Some(api) = session.client() else {
            return;
        };
        let provider = session.provider_name().unwrap_or_default();

        let (progress, mut steps) = tokio::sync::mpsc::unbounded_channel();
        let job = self.io.spawn(run(api, provider, parent, names, progress));
        self.state = ExportState::Reading { done: 0, total: 0 };
        cx.notify();

        // Dropping this task drops `steps`, and the job stops at its next step.
        self.task = Some(cx.spawn(async move |this, cx| {
            while let Some(step) = steps.recv().await {
                let applied = this.update(cx, |this, cx| {
                    this.state = step;
                    cx.notify();
                });
                if applied.is_err() {
                    return;
                }
            }
            let finished = join(job).await;
            this.update(cx, |this, cx| {
                this.state = match finished {
                    Ok(summary) => {
                        log::info!(
                            "export: wrote {} lists and {} tracks to {}",
                            summary.lists,
                            summary.tracks,
                            summary.folder.display()
                        );
                        let name = summary
                            .folder
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        Toasts::about(Outcome::Done, "toast-library-exported", name, cx);
                        cx.reveal_path(&summary.folder);
                        ExportState::Done {
                            folder: summary.folder,
                            lists: summary.lists,
                            tracks: summary.tracks,
                            skipped: summary.skipped,
                        }
                    }
                    Err(error) => {
                        log::warn!("export: {error:#}");
                        Toasts::show(Outcome::Failed, "toast-library-export-failed", cx);
                        ExportState::Failed(format!("{error:#}"))
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }
}

/// Reads the liked songs and every playlist one after another, so a big library never puts
/// a burst of requests on the provider, then writes the files on a blocking thread.
async fn run(
    api: Arc<dyn MusicApi>,
    provider: &'static str,
    parent: PathBuf,
    names: ExportNames,
    progress: UnboundedSender<ExportState>,
) -> Result<Summary> {
    let playlists = api.playlists().await.context("cannot list the playlists")?;
    let total = playlists.len() + 1;
    step(&progress, ExportState::Reading { done: 0, total })?;

    let liked = api
        .saved_tracks()
        .await
        .context("cannot read the liked songs")?;
    step(&progress, ExportState::Reading { done: 1, total })?;

    let mut lists = Vec::with_capacity(playlists.len());
    let mut skipped = 0;
    for (index, playlist) in playlists.into_iter().enumerate() {
        match playlist_tracks(api.as_ref(), &playlist.id).await {
            Ok(tracks) => lists.push(list(api.as_ref(), playlist, &tracks)),
            Err(error) => {
                log::warn!("export: cannot read playlist {}: {error:#}", playlist.id);
                skipped += 1;
            }
        }
        step(
            &progress,
            ExportState::Reading {
                done: index + 2,
                total,
            },
        )?;
    }
    step(&progress, ExportState::Writing)?;

    let now = OffsetDateTime::now_utc();
    let archive = Archive {
        exported_at: now
            .format(&Rfc3339)
            .context("cannot format the export time")?,
        provider,
        liked_songs: rows(api.as_ref(), &liked),
        playlists: lists,
    };
    let tracks = archive.liked_songs.len()
        + archive
            .playlists
            .iter()
            .map(|list| list.tracks.len())
            .sum::<usize>();
    let lists = archive.playlists.len() + 1;
    let base = format!("{} {}", names.folder, now.date());
    let folder =
        tokio::task::spawn_blocking(move || write(&parent, &base, &names, &archive)).await??;

    Ok(Summary {
        folder,
        lists,
        tracks,
        skipped,
    })
}

/// Hands a step to the entity. Failing means the entity dropped the export, so the job ends.
fn step(progress: &UnboundedSender<ExportState>, state: ExportState) -> Result<()> {
    progress
        .send(state)
        .ok()
        .context("the export was cancelled")
}

/// Every track of a playlist, following the provider's continuations to the end.
async fn playlist_tracks(api: &dyn MusicApi, playlist_id: &str) -> Result<Vec<Track>> {
    let detail = api.playlist(playlist_id).await?;
    let mut tracks = detail.tracks;
    let mut next = detail.continuation;
    while let Some(token) = next {
        let (more, after) = api.playlist_continuation(&token).await?;
        // A provider that keeps answering with nothing would otherwise be asked forever.
        if more.is_empty() {
            break;
        }
        tracks.extend(more);
        next = after;
    }
    Ok(tracks)
}

fn list(api: &dyn MusicApi, playlist: Playlist, tracks: &[Track]) -> List {
    List {
        url: api.share_url(MediaKind::Playlist, &playlist.id),
        id: playlist.id,
        name: playlist.name,
        owner: playlist.owner,
        collaborative: playlist.collaborative,
        public: playlist.public,
        tracks: rows(api, tracks),
    }
}

fn rows(api: &dyn MusicApi, tracks: &[Track]) -> Vec<Row> {
    tracks
        .iter()
        .enumerate()
        .map(|(index, track)| Row {
            position: index + 1,
            title: track.name.clone(),
            artists: match track.artist_refs.is_empty() {
                true => vec![track.artists.clone()],
                false => track
                    .artist_refs
                    .iter()
                    .map(|artist| artist.name.clone())
                    .collect(),
            },
            album: track.album.clone(),
            duration_ms: u64::try_from(track.duration.as_millis()).unwrap_or(u64::MAX),
            isrc: track.isrc.clone(),
            added_at: track.added_at.and_then(stamp),
            added_by: track.added_by.as_ref().map(|added| added.name.clone()),
            explicit: track.explicit,
            popularity: track.popularity,
            url: track
                .id
                .as_deref()
                .and_then(|id| api.share_url(MediaKind::Track, id)),
            id: track.id.clone(),
            album_id: track.album_id.clone(),
        })
        .collect()
}

/// Writes the export into a new folder named `base` inside `parent` and answers with it.
fn write(parent: &Path, base: &str, names: &ExportNames, archive: &Archive) -> Result<PathBuf> {
    let folder = fresh_folder(parent, &file_name(base).unwrap_or_else(|| "Sonora".into()))?;

    let liked = file_name(&names.liked).unwrap_or_else(|| "Liked Songs".into());
    write_csv(&folder.join(format!("{liked}.csv")), &archive.liked_songs)?;

    if !archive.playlists.is_empty() {
        let playlists = file_name(&names.playlists).unwrap_or_else(|| "Playlists".into());
        let dir = folder.join(playlists);
        fs::create_dir(&dir).context("cannot create the playlists folder")?;
        let mut taken = HashSet::new();
        for list in &archive.playlists {
            let name = file_name(&list.name).unwrap_or_else(|| list.id.clone());
            let name = unique(&mut taken, name);
            write_csv(&dir.join(format!("{name}.csv")), &list.tracks)?;
        }
    }

    let file = fs::File::create(folder.join(ARCHIVE)).context("cannot create library.json")?;
    let mut out = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut out, archive).context("cannot write library.json")?;
    out.flush().context("cannot write library.json")?;

    Ok(folder)
}

/// Creates `parent/base`, or `parent/base (2)` and so on when that is taken, so an export
/// never writes into an earlier one.
fn fresh_folder(parent: &Path, base: &str) -> Result<PathBuf> {
    for attempt in 1..=ATTEMPTS {
        let name = match attempt {
            1 => base.to_owned(),
            _ => format!("{base} ({attempt})"),
        };
        let folder = parent.join(name);
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", folder.display()));
            }
        }
    }
    anyhow::bail!("cannot find a free folder name in {}", parent.display())
}

fn write_csv(path: &Path, rows: &[Row]) -> Result<()> {
    let file =
        fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut out = BufWriter::new(file);
    let mut text = String::from(BOM);
    text.push_str(&COLUMNS.join(","));
    text.push_str("\r\n");
    out.write_all(text.as_bytes())?;

    for row in rows {
        let fields = [
            row.position.to_string(),
            cell(&row.title).into_owned(),
            cell(&row.artists.join("; ")).into_owned(),
            cell(&row.album).into_owned(),
            row.duration_ms.to_string(),
            cell(row.isrc.as_deref().unwrap_or_default()).into_owned(),
            row.added_at.clone().unwrap_or_default(),
            cell(row.added_by.as_deref().unwrap_or_default()).into_owned(),
            row.explicit.to_string(),
            row.popularity.to_string(),
            cell(row.url.as_deref().unwrap_or_default()).into_owned(),
            cell(row.id.as_deref().unwrap_or_default()).into_owned(),
            cell(row.album_id.as_deref().unwrap_or_default()).into_owned(),
        ];
        let mut line = fields.join(",");
        line.push_str("\r\n");
        out.write_all(line.as_bytes())?;
    }

    out.flush()
        .with_context(|| format!("cannot write {}", path.display()))
}

/// One text cell, quoted when it holds a comma, a quote or a line break. A cell a spreadsheet
/// would run as a formula gets a leading apostrophe, since playlist names and titles come
/// from strangers; `library.json` keeps the text as it was.
fn cell(value: &str) -> Cow<'_, str> {
    let value: Cow<'_, str> = match value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        true => format!("'{value}").into(),
        false => value.into(),
    };
    match value.contains([',', '"', '\n', '\r']) {
        true => format!("\"{}\"", value.replace('"', "\"\"")).into(),
        false => value,
    }
}

/// A file name every desktop system accepts, made from `name`, or `None` when nothing of it is
/// left.
fn file_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(NAME_LIMIT)
        .collect();
    let mut cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_owned();
    if cleaned.is_empty() {
        return None;
    }
    let stem = cleaned.split('.').next().unwrap_or_default();
    if RESERVED.contains(&stem.to_ascii_uppercase().as_str()) {
        cleaned.push('_');
    }
    Some(cleaned)
}

/// `name`, or `name (2)` and so on when an earlier file took it. Windows and macOS ignore
/// case, so `Mix` and `mix` count as the same name.
fn unique(taken: &mut HashSet<String>, name: String) -> String {
    let mut candidate = name.clone();
    let mut attempt = 1;
    while !taken.insert(candidate.to_lowercase()) {
        attempt += 1;
        candidate = format!("{name} ({attempt})");
    }
    candidate
}

fn stamp(seconds: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .format(&Rfc3339)
        .ok()
}
