# bippi 🎵

Download music from YouTube and other sources with ease!

## ✨ Features

- 🎤 **Single Track Downloads** - Download individual songs by URL, search query, or alias
- 💿 **Album Downloads** - Download entire albums or playlists with automatic metadata
- 🎯 **MusicBrainz Integration** - Fetch accurate album information and track metadata
- 🏷️ **Aliases** - Create shortcuts for frequently downloaded URLs
- ⚙️ **Configuration** - Set default download directories and preferences
- 🎼 **Multiple Formats** - Support for mp3, m4a, flac, and more

## 📦 Installation

Install directly from GitHub using Cargo:

```bash
cargo install --git https://github.com/stevecellbio/bippi
```

### 📋 Requirements

- [yt-dlp](https://github.com/yt-dlp/yt-dlp) must be installed and available in your PATH

## 🚀 Usage

### Download a single track

```bash
# By search query
bippi single Metallica - Nothing Else Matters

# By URL
bippi single https://www.youtube.com/watch?v=tAGnKpE4NCI

# Using an alias
bippi single my-favorite-song
```

### Download an album

```bash
# Search by artist and album name
bippi album Metallica - Master of Puppets

# From a playlist URL
bippi album https://www.youtube.com/playlist?list=PLxxx

# Using an alias
bippi album my-album
```

### Create aliases 🏷️

```bash
# Create an alias for a single track
bippi alias add focus https://www.youtube.com/watch?v=abc123

# Create an alias for an album
bippi alias add chill-album https://www.youtube.com/playlist?list=PLxxx --album

# List all aliases
bippi alias list

# Remove an alias
bippi alias remove focus
```

### Configure settings ⚙️

```bash
# Set default download directory
bippi config set-dest ~/Music

# Show current configuration
bippi config show

# Clear default destination
bippi config clear-dest
```

### Specify output format and destination 🎛️

```bash
# Download as FLAC to a specific directory
bippi single Metallica - Nothing Else Matters -f flac -d ~/Downloads

# Download album as m4a
bippi album Metallica - Master of Puppets -f m4a
```

## 📝 License

MIT
