# Netease Music Deduplication
Deduplication of locally cached song files in NetEase Cloud Music

First judge whether it is duplicated according to the music id in the file.
For files without a music id, it is judged based on the title and album.
If there is no tag, the file name will be used as the title, and those within 1.5 seconds of the duration will be regarded as duplicates.
Music files with tags will be reserved first, and the judgment of music without tags does not guarantee reliability.

# Usage
```text
USAGE:
     netease-music-deduplication.exe [OPTIONS]

OPTIONS:
     -d, --dry-run           Do not output any files, only view the running results
     -h, --help              Print help information
     -i, --input <INPUT>...  Input media file path
     -o, --output <OUTPUT>   Save the deduplicated media files to this path
     -V, --version           Print version information
```