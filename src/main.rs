use clap::{Args, Parser, Subcommand};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use urlencoding::encode;

const APP_NAME: &str = "bippi";
const CONFIG_FILENAME: &str = "config.json";
const MUSICBRAINZ_BASE_URL: &str = "https://musicbrainz.org/ws/2";
const MUSICBRAINZ_USER_AGENT: &str = "bippi/0.1.0 (https://github.com/landonrogers/bippi)";

type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("{0}")]
    Message(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Config directory not found")]
    MissingConfigDir,
    #[error("Failed to parse config: {0}")]
    ConfigParse(#[from] serde_json::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MusicBrainz did not return any release for '{0}'")]
    MusicBrainzNotFound(String),
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut config = AppConfig::load()?;

    match cli.command {
        Commands::Single(args) => handle_download(args, &config, DownloadMode::Single),
        Commands::Album(args) => handle_download(args, &config, DownloadMode::Album),
        Commands::Alias { command } => {
            let changed = handle_alias(command, &mut config)?;
            if changed {
                config.save()?;
            }
            Ok(())
        }
        Commands::Config { command } => {
            let changed = handle_config(command, &mut config)?;
            if changed {
                config.save()?;
            }
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DownloadMode {
    Single,
    Album,
}

fn handle_download(args: DownloadArgs, config: &AppConfig, mode: DownloadMode) -> Result<()> {
    let DownloadArgs {
        target,
        dest,
        format,
    } = args;

    let joined_target = target.join(" ");
    let query = joined_target.trim();
    let query_owned = query.to_string();

    let destination = if let Some(dest) = dest {
        ensure_absolute(&dest)?
    } else if let Some(config_dest) = &config.default_destination {
        config_dest.clone()
    } else {
        std::env::current_dir()?
    };

    fs::create_dir_all(&destination)?;

    let alias_entry = config.aliases.get(query);
    let album_mode = matches!(mode, DownloadMode::Album);

    if album_mode && alias_entry.is_none() && !looks_like_url(query) {
        match download_album_with_musicbrainz(query, &destination, &format) {
            Ok(()) => return Ok(()),
            Err(AppError::MusicBrainzNotFound(_)) => {
                println!(
                    "MusicBrainz did not find a matching release; falling back to YouTube search"
                );
            }
            Err(err) => return Err(err),
        }
    }

    let (resolved_target, alias_album) = if let Some(alias) = alias_entry {
        println!("using alias '{}' -> {}", query, alias.url);
        (alias.url.clone(), alias.album)
    } else if looks_like_url(query) {
        (query_owned.clone(), false)
    } else {
        match mode {
            DownloadMode::Single => {
                println!("searching YouTube for '{}' (first match)", query);
                (build_single_search_query(query), false)
            }
            DownloadMode::Album => {
                let resolved = resolve_album_query(query)?;
                (resolved, false)
            }
        }
    };

    let download_album = alias_album || album_mode;

    let output_template = destination.join("%(title)s.%(ext)s");
    let output_template = output_template.to_string_lossy().to_string();

    let mut command = base_yt_dlp_command(&format, &output_template);

    if download_album {
        command.arg("--yes-playlist");
    } else {
        command.arg("--no-playlist");
    }

    if should_apply_album_metadata(download_album, &resolved_target) {
        command
            .arg("--parse-metadata")
            .arg("%(playlist_title|)s:%(meta_album)s")
            .arg("--parse-metadata")
            .arg("%(playlist_index)02d:%(meta_track_number)s");
    }

    command.arg(&resolved_target);

    println!("saving audio to {} as {}", destination.display(), format);
    run_yt_dlp(command)
}

fn base_yt_dlp_command(format: &str, output_template: &str) -> Command {
    let mut command = Command::new("yt-dlp");
    command
        .arg("--ignore-errors")
        .arg("--continue")
        .arg("-x")
        .arg("--audio-format")
        .arg(format)
        .arg("--output")
        .arg(output_template)
        .arg("--embed-metadata");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

fn run_yt_dlp(mut command: Command) -> Result<()> {
    let status = command.status().map_err(map_yt_dlp_error)?;

    if status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "yt-dlp exited with status {}",
            status.code().unwrap_or(-1)
        )))
    }
}

fn resolve_album_query(query: &str) -> Result<String> {
    println!("searching YouTube for album '{}'", query);

    match find_album_playlist(query)? {
        Some(url) => {
            println!("found playlist match: {}", url);
            Ok(url)
        }
        None => {
            println!(
                "no playlist found for '{}'; falling back to first search result",
                query
            );
            Ok(build_single_search_query(query))
        }
    }
}

fn find_album_playlist(query: &str) -> Result<Option<String>> {
    let search_term = format!("ytsearch10:{} album", query);
    let output = Command::new("yt-dlp")
        .arg("--flat-playlist")
        .arg("-J")
        .arg(&search_term)
        .stdin(Stdio::null())
        .output()
        .map_err(map_yt_dlp_error)?;

    if !output.status.success() {
        return Ok(None);
    }

    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let entries = match parsed.get("entries").and_then(|value| value.as_array()) {
        Some(entries) => entries,
        None => return Ok(None),
    };

    for entry in entries {
        if let Some(url) = playlist_url_from_entry(entry) {
            return Ok(Some(url));
        }
    }

    Ok(None)
}

fn playlist_url_from_entry(entry: &serde_json::Value) -> Option<String> {
    let entry_type = entry.get("_type").and_then(|v| v.as_str());
    let ie_key = entry.get("ie_key").and_then(|v| v.as_str());
    let url = entry.get("url").and_then(|v| v.as_str());
    let playlist_id = entry.get("playlist_id").and_then(|v| v.as_str());
    let id = entry.get("id").and_then(|v| v.as_str());
    let fallback_id = playlist_id.or(id);

    if let Some(url) = url {
        if url.contains("://") && url.contains("list=") {
            return Some(url.to_string());
        }

        if matches!(entry_type, Some("playlist"))
            || matches!(
                ie_key,
                Some("YoutubeTab" | "YoutubePlaylist" | "YoutubeMix")
            )
        {
            return Some(normalize_playlist_url(url, fallback_id));
        }
    }

    if let Some(id) = fallback_id {
        if id.starts_with("PL") || id.starts_with("OL") || id.starts_with("RD") {
            return Some(format!("https://www.youtube.com/playlist?list={id}"));
        }
    }

    None
}

fn normalize_playlist_url(url: &str, fallback_id: Option<&str>) -> String {
    if url.contains("://") {
        url.to_string()
    } else if url.starts_with("/playlist?") {
        format!("https://www.youtube.com{url}")
    } else if url.starts_with("playlist?") {
        format!("https://www.youtube.com/{url}")
    } else if url.starts_with("/watch?") {
        format!("https://www.youtube.com{url}")
    } else if url.starts_with("watch?") {
        format!("https://www.youtube.com/{url}")
    } else if let Some(id) = fallback_id {
        format!("https://www.youtube.com/playlist?list={id}")
    } else {
        format!("https://www.youtube.com/playlist?list={url}")
    }
}

fn map_yt_dlp_error(err: std::io::Error) -> AppError {
    if err.kind() == ErrorKind::NotFound {
        AppError::Message(
            "yt-dlp was not found in PATH. Install it from https://github.com/yt-dlp/yt-dlp and try again.".to_string(),
        )
    } else {
        AppError::Io(err)
    }
}

fn download_album_with_musicbrainz(query: &str, destination: &Path, format: &str) -> Result<()> {
    println!("saving audio to {} as {}", destination.display(), format);
    println!("searching MusicBrainz for album '{}'", query);

    let client = MusicBrainzClient::new()?;
    let album = match client.find_album(query)? {
        Some(album) => album,
        None => return Err(AppError::MusicBrainzNotFound(query.to_string())),
    };

    println!(
        "found release: {} - {} ({} track{})",
        album.artist,
        album.title,
        album.tracks.len(),
        if album.tracks.len() == 1 { "" } else { "s" }
    );

    let total_tracks = album.tracks.len();
    for track in &album.tracks {
        let progress = format!("[{}/{}]", track.overall_index, total_tracks);
        println!(
            "{} searching YouTube for '{} - {}'",
            progress, album.artist, track.title
        );

        let search_terms = format!("{} {} {}", album.artist, track.title, album.title);
        let yt_query = build_single_search_query(&search_terms);
        let output_template = track_output_template(destination, track, album.total_discs);
        let metadata_args = build_metadata_args(&album, track, total_tracks);

        let mut command = base_yt_dlp_command(format, &output_template);
        command.arg("--no-playlist");
        command.arg("--postprocessor-args").arg(metadata_args);
        command.arg(&yt_query);

        run_yt_dlp(command)?;
    }

    Ok(())
}

struct MusicBrainzClient {
    client: Client,
}

impl MusicBrainzClient {
    fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(MUSICBRAINZ_USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self { client })
    }

    fn find_album(&self, query: &str) -> Result<Option<MusicBrainzAlbum>> {
        let search_query = build_musicbrainz_search_query(query);
        let search_url = format!(
            "{}/release/?query={}&fmt=json&limit=1",
            MUSICBRAINZ_BASE_URL,
            encode(&search_query)
        );

        let search_response: MbReleaseSearchResponse = self
            .client
            .get(&search_url)
            .header("Accept", "application/json")
            .send()?
            .error_for_status()?
            .json()?;

        let Some(release) = search_response.releases.into_iter().next() else {
            return Ok(None);
        };

        let release_id = release.id;
        let detail_url = format!(
            "{}/release/{}?inc=recordings+artist-credits&fmt=json",
            MUSICBRAINZ_BASE_URL, release_id
        );

        let detail: MbReleaseDetail = self
            .client
            .get(&detail_url)
            .header("Accept", "application/json")
            .send()?
            .error_for_status()?
            .json()?;

        convert_release_detail(detail).map(Some)
    }
}

fn build_musicbrainz_search_query(raw: &str) -> String {
    if let Some((artist, album)) = split_artist_album(raw) {
        format!(
            "release:\"{}\" AND artist:\"{}\"",
            escape_musicbrainz_query(&album),
            escape_musicbrainz_query(&artist)
        )
    } else {
        raw.to_string()
    }
}

fn split_artist_album(raw: &str) -> Option<(String, String)> {
    for delimiter in ['-', '\u{2013}', '\u{2014}'] {
        if let Some((artist, album)) = raw.split_once(delimiter) {
            let artist = artist.trim();
            let album = album.trim();
            if !artist.is_empty() && !album.is_empty() {
                return Some((artist.to_string(), album.to_string()));
            }
        }
    }
    None
}

fn escape_musicbrainz_query(value: &str) -> String {
    value.replace('"', "\\\"")
}

fn convert_release_detail(detail: MbReleaseDetail) -> Result<MusicBrainzAlbum> {
    let MbReleaseDetail {
        title,
        date,
        artist_credit,
        media,
    } = detail;

    let album_title = title.unwrap_or_else(|| "Unknown Release".to_string());
    let artist = {
        let formatted = format_artist_credit(&artist_credit);
        if formatted.is_empty() {
            "Unknown Artist".to_string()
        } else {
            formatted
        }
    };

    let mut tracks = Vec::new();
    let mut discs_with_tracks = 0u32;

    for (medium_index, medium) in media.into_iter().enumerate() {
        if medium.tracks.is_empty() {
            continue;
        }
        discs_with_tracks += 1;
        let disc_number = medium.position.unwrap_or((medium_index + 1) as u32);
        for (index_on_disc, track) in medium.tracks.into_iter().enumerate() {
            let title = track
                .title
                .or_else(|| track.recording.and_then(|rec| rec.title))
                .unwrap_or_else(|| format!("Track {}", index_on_disc + 1));
            let position = track
                .position
                .or_else(|| track.number.and_then(|num| num.parse::<u32>().ok()))
                .unwrap_or((index_on_disc + 1) as u32);
            let overall_index = tracks.len() + 1;
            tracks.push(MusicBrainzTrack {
                title,
                disc: disc_number,
                position,
                overall_index,
            });
        }
    }

    if tracks.is_empty() {
        return Err(AppError::Message(
            "MusicBrainz release does not contain any tracks".to_string(),
        ));
    }

    let total_discs = if discs_with_tracks == 0 {
        1
    } else {
        discs_with_tracks
    };

    Ok(MusicBrainzAlbum {
        title: album_title,
        artist,
        release_date: date,
        total_discs,
        tracks,
    })
}

fn format_artist_credit(credits: &[MbArtistCredit]) -> String {
    if credits.is_empty() {
        return String::new();
    }

    let mut composed = String::new();
    for credit in credits {
        if let Some(name) = credit.name.as_deref().or_else(|| {
            credit
                .artist
                .as_ref()
                .and_then(|artist| artist.name.as_deref())
        }) {
            composed.push_str(name);
        }
        if let Some(join) = credit.joinphrase.as_deref() {
            composed.push_str(join);
        }
    }

    if composed.is_empty() {
        credits
            .iter()
            .filter_map(|credit| {
                credit
                    .artist
                    .as_ref()
                    .and_then(|artist| artist.name.clone())
            })
            .collect::<Vec<_>>()
            .join(" & ")
    } else {
        composed
    }
}

fn track_output_template(destination: &Path, track: &MusicBrainzTrack, total_discs: u32) -> String {
    let prefix = if total_discs > 1 {
        format!("{:02}-{:02}", track.disc, track.position)
    } else {
        format!("{:02}", track.overall_index)
    };
    let safe_title = sanitize_filename(&track.title);
    let file_name = format!("{} - {}.%(ext)s", prefix, safe_title);
    destination.join(file_name).to_string_lossy().to_string()
}

fn build_metadata_args(
    album: &MusicBrainzAlbum,
    track: &MusicBrainzTrack,
    total_tracks: usize,
) -> String {
    let mut parts = vec![
        format!("-metadata artist={}", quote_metadata_value(&album.artist)),
        format!("-metadata album={}", quote_metadata_value(&album.title)),
        format!(
            "-metadata album_artist={}",
            quote_metadata_value(&album.artist)
        ),
        format!("-metadata title={}", quote_metadata_value(&track.title)),
        format!(
            "-metadata track={}",
            quote_metadata_value(&format!("{:02}/{}", track.overall_index, total_tracks))
        ),
    ];

    if album.total_discs > 1 {
        parts.push(format!(
            "-metadata disc={}",
            quote_metadata_value(&track.disc.to_string())
        ));
    }

    if let Some(date) = &album.release_date {
        parts.push(format!("-metadata date={}", quote_metadata_value(date)));
    }

    format!("ffmpeg:{}", parts.join(" "))
}

fn quote_metadata_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn sanitize_filename(input: &str) -> String {
    let mut sanitized = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '/' | '\\' | '?' | '*' | '"' | '<' | '>' | '|' | ':' => sanitized.push('_'),
            c if c.is_control() => sanitized.push('_'),
            _ => sanitized.push(ch),
        }
    }
    let trimmed = sanitized.trim().trim_matches('.');
    if trimmed.is_empty() {
        "track".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug)]
struct MusicBrainzAlbum {
    title: String,
    artist: String,
    release_date: Option<String>,
    total_discs: u32,
    tracks: Vec<MusicBrainzTrack>,
}

#[derive(Debug)]
struct MusicBrainzTrack {
    title: String,
    disc: u32,
    position: u32,
    overall_index: usize,
}

#[derive(Debug, Deserialize)]
struct MbReleaseSearchResponse {
    #[serde(default)]
    releases: Vec<MbReleaseSearchEntry>,
}

#[derive(Debug, Deserialize)]
struct MbReleaseSearchEntry {
    id: String,
}

#[derive(Debug, Deserialize)]
struct MbReleaseDetail {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<MbArtistCredit>,
    #[serde(default)]
    media: Vec<MbMedium>,
}

#[derive(Debug, Deserialize)]
struct MbArtistCredit {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    joinphrase: Option<String>,
    #[serde(default)]
    artist: Option<MbArtist>,
}

#[derive(Debug, Deserialize)]
struct MbArtist {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MbMedium {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    tracks: Vec<MbTrack>,
}

#[derive(Debug, Deserialize)]
struct MbTrack {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    number: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    recording: Option<MbRecording>,
}

#[derive(Debug, Deserialize)]
struct MbRecording {
    #[serde(default)]
    title: Option<String>,
}

fn looks_like_url(input: &str) -> bool {
    let lowered = input.trim().to_ascii_lowercase();
    lowered.starts_with("http://")
        || lowered.starts_with("https://")
        || lowered.starts_with("ytsearch:")
        || lowered.starts_with("ytsearch")
        || lowered.starts_with("www.")
        || lowered.contains("://")
}

fn should_apply_album_metadata(download_album: bool, resolved_target: &str) -> bool {
    download_album && looks_like_playlist(resolved_target)
}

fn looks_like_playlist(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    lowered.contains("list=")
}

fn build_single_search_query(query: &str) -> String {
    let trimmed = query.trim();

    // If query contains artist - song format, preserve it for better search results
    let search_query = if let Some((artist, song)) = split_artist_song(trimmed) {
        format!("{} {}", artist, song)
    } else {
        trimmed.to_string()
    };

    let mut terms = String::with_capacity(search_query.len() + 24);
    terms.push_str(&search_query);

    if !search_query.to_ascii_lowercase().contains("audio") {
        terms.push_str(" audio");
    }

    terms.push_str(" -\"music video\"");

    format!("ytsearch1:{}", terms.trim())
}

fn split_artist_song(raw: &str) -> Option<(String, String)> {
    for delimiter in ['-', '\u{2013}', '\u{2014}'] {
        if let Some((artist, song)) = raw.split_once(delimiter) {
            let artist = artist.trim();
            let song = song.trim();
            if !artist.is_empty() && !song.is_empty() {
                return Some((artist.to_string(), song.to_string()));
            }
        }
    }
    None
}

fn handle_alias(command: AliasCommand, config: &mut AppConfig) -> Result<bool> {
    match command {
        AliasCommand::Add(args) => {
            let entry = AliasEntry {
                url: args.url,
                album: args.album,
            };
            let existed = config.aliases.insert(args.name.clone(), entry).is_some();
            if existed {
                println!("updated alias '{}'", args.name);
            } else {
                println!("created alias '{}'", args.name);
            }
            Ok(true)
        }
        AliasCommand::Remove(args) => {
            if config.aliases.remove(&args.name).is_some() {
                println!("removed alias '{}'", args.name);
                Ok(true)
            } else {
                Err(AppError::Message(format!(
                    "alias '{}' not found",
                    args.name
                )))
            }
        }
        AliasCommand::List => {
            if config.aliases.is_empty() {
                println!("no aliases defined yet");
            } else {
                for (name, entry) in &config.aliases {
                    if entry.album {
                        println!("{} -> {} (album)", name, entry.url);
                    } else {
                        println!("{} -> {}", name, entry.url);
                    }
                }
            }
            Ok(false)
        }
    }
}

fn handle_config(command: ConfigCommand, config: &mut AppConfig) -> Result<bool> {
    match command {
        ConfigCommand::SetDest(args) => {
            let absolute = ensure_absolute(&args.path)?;
            if let Some(parent) = absolute.parent() {
                fs::create_dir_all(parent)?;
            }
            if !absolute.exists() {
                fs::create_dir_all(&absolute)?;
            }
            config.default_destination = Some(absolute.clone());
            println!("default destination set to {}", absolute.display());
            Ok(true)
        }
        ConfigCommand::Show => {
            match &config.default_destination {
                Some(path) => println!("default destination: {}", path.display()),
                None => println!("default destination: not set"),
            }
            if config.aliases.is_empty() {
                println!("aliases: none");
            } else {
                println!("aliases: {}", config.aliases.len());
            }
            Ok(false)
        }
        ConfigCommand::ClearDest => {
            if config.default_destination.take().is_some() {
                println!("cleared default destination");
                Ok(true)
            } else {
                println!("default destination was already unset");
                Ok(false)
            }
        }
    }
}

fn ensure_absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AppConfig {
    #[serde(default)]
    default_destination: Option<PathBuf>,
    #[serde(default)]
    aliases: BTreeMap<String, AliasEntry>,
}

impl AppConfig {
    fn load() -> Result<Self> {
        let path = config_file_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = fs::read(&path)?;
        if data.is_empty() {
            return Ok(Self::default());
        }
        let mut config: Self = serde_json::from_slice(&data)?;
        if config.default_destination.is_none() {
            config.default_destination = default_music_dir();
        }
        Ok(config)
    }

    fn save(&self) -> Result<()> {
        let path = config_file_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_destination: default_music_dir(),
            aliases: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct AliasEntry {
    url: String,
    #[serde(default)]
    album: bool,
}

fn config_file_path() -> Result<PathBuf> {
    let mut base = dirs::config_dir().ok_or(AppError::MissingConfigDir)?;
    base.push(APP_NAME);
    base.push(CONFIG_FILENAME);
    Ok(base)
}

fn default_music_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join("music"))
}

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Download music from YouTube and other sources",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Download a single track using a URL, alias, or search
    Single(DownloadArgs),
    /// Download an entire album/playlist
    Album(DownloadArgs),
    /// Manage human-friendly aliases for URLs
    Alias {
        #[command(subcommand)]
        command: AliasCommand,
    },
    /// Configure default download settings
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Args, Debug)]
struct DownloadArgs {
    /// URL, alias name, or free-form search query
    #[arg(value_name = "TARGET", num_args = 1..)]
    target: Vec<String>,
    /// Destination directory for the downloaded audio
    #[arg(short, long)]
    dest: Option<PathBuf>,
    /// Audio format (mp3, m4a, flac ...)
    #[arg(short, long, default_value = "mp3")]
    format: String,
}

#[derive(Subcommand, Debug)]
enum AliasCommand {
    /// Create or update an alias mapped to a URL
    Add(AliasAddArgs),
    /// Remove an alias
    Remove(AliasRemoveArgs),
    /// List all aliases
    List,
}

#[derive(Args, Debug)]
struct AliasAddArgs {
    /// Short name for the alias (e.g. "focus")
    name: String,
    /// URL that the alias resolves to
    url: String,
    /// Mark the alias as an album/playlist
    #[arg(long)]
    album: bool,
}

#[derive(Args, Debug)]
struct AliasRemoveArgs {
    /// Alias name to remove
    name: String,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Set the default download destination directory
    SetDest(ConfigSetDestArgs),
    /// Show the current configuration
    Show,
    /// Clear the default download destination
    ClearDest,
}

#[derive(Args, Debug)]
struct ConfigSetDestArgs {
    /// Directory path where downloads should be saved by default
    path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_artist_album() {
        assert_eq!(
            split_artist_album("Metallica - Master of Puppets"),
            Some(("Metallica".to_string(), "Master of Puppets".to_string()))
        );
        assert_eq!(
            split_artist_album("Foo Fighters - The Colour and the Shape"),
            Some(("Foo Fighters".to_string(), "The Colour and the Shape".to_string()))
        );
        assert_eq!(split_artist_album("NoDelimiterHere"), None);
        assert_eq!(split_artist_album("- OnlyAlbum"), None);
        assert_eq!(split_artist_album("OnlyArtist -"), None);
    }

    #[test]
    fn test_split_artist_song() {
        assert_eq!(
            split_artist_song("Metallica - Nothing Else Matters"),
            Some(("Metallica".to_string(), "Nothing Else Matters".to_string()))
        );
        assert_eq!(
            split_artist_song("Foo Fighters - Everlong"),
            Some(("Foo Fighters".to_string(), "Everlong".to_string()))
        );
        assert_eq!(split_artist_song("JustASongTitle"), None);
    }

    #[test]
    fn test_looks_like_url() {
        assert!(looks_like_url("https://www.youtube.com/watch?v=123"));
        assert!(looks_like_url("http://example.com"));
        assert!(looks_like_url("ytsearch:something"));
        assert!(looks_like_url("www.youtube.com"));
        assert!(!looks_like_url("just a search query"));
        assert!(!looks_like_url("Metallica - Nothing Else Matters"));
    }

    #[test]
    fn test_looks_like_playlist() {
        assert!(looks_like_playlist("https://www.youtube.com/playlist?list=PLxxx"));
        assert!(looks_like_playlist("https://www.youtube.com/watch?v=123&list=PLyyy"));
        assert!(!looks_like_playlist("https://www.youtube.com/watch?v=123"));
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("Normal Title"), "Normal Title");
        assert_eq!(sanitize_filename("Title/With\\Slashes"), "Title_With_Slashes");
        assert_eq!(sanitize_filename("Title:With*Special?Chars"), "Title_With_Special_Chars");
        assert_eq!(sanitize_filename("  Trimmed  "), "Trimmed");
        assert_eq!(sanitize_filename("...dots..."), "dots");
        assert_eq!(sanitize_filename(""), "track");
    }

    #[test]
    fn test_build_single_search_query() {
        let query = build_single_search_query("Metallica - Nothing Else Matters");
        assert!(query.starts_with("ytsearch1:"));
        assert!(query.contains("Metallica"));
        assert!(query.contains("Nothing Else Matters"));
        assert!(query.contains("audio"));
        assert!(query.contains("-\"music video\""));

        let query2 = build_single_search_query("some audio track");
        assert!(!query2.contains("audio audio"));
    }

    #[test]
    fn test_escape_musicbrainz_query() {
        assert_eq!(escape_musicbrainz_query("Normal Text"), "Normal Text");
        assert_eq!(escape_musicbrainz_query("Text \"with\" quotes"), "Text \\\"with\\\" quotes");
    }

    #[test]
    fn test_build_musicbrainz_search_query() {
        let query = build_musicbrainz_search_query("Metallica - Master of Puppets");
        assert!(query.contains("release:\"Master of Puppets\""));
        assert!(query.contains("artist:\"Metallica\""));

        let query2 = build_musicbrainz_search_query("just a query");
        assert_eq!(query2, "just a query");
    }

    #[test]
    fn test_normalize_playlist_url() {
        assert_eq!(
            normalize_playlist_url("https://youtube.com/playlist?list=123", None),
            "https://youtube.com/playlist?list=123"
        );
        assert_eq!(
            normalize_playlist_url("/playlist?list=123", None),
            "https://www.youtube.com/playlist?list=123"
        );
        assert_eq!(
            normalize_playlist_url("playlist?list=123", None),
            "https://www.youtube.com/playlist?list=123"
        );
        assert_eq!(
            normalize_playlist_url("PL123", Some("PL123")),
            "https://www.youtube.com/playlist?list=PL123"
        );
    }

    #[test]
    fn test_format_artist_credit() {
        let credits = vec![
            MbArtistCredit {
                name: Some("Artist One".to_string()),
                joinphrase: Some(" & ".to_string()),
                artist: None,
            },
            MbArtistCredit {
                name: Some("Artist Two".to_string()),
                joinphrase: None,
                artist: None,
            },
        ];
        assert_eq!(format_artist_credit(&credits), "Artist One & Artist Two");

        let empty: Vec<MbArtistCredit> = vec![];
        assert_eq!(format_artist_credit(&empty), "");
    }
}
