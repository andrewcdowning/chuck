# Spec: media cards

Status: phase 1 (photos) built in PR 11. Decisions taken: D1 files in a folder next to the database; D2 only the resized,
metadata-stripped copy is kept, no originals; D3 HEIC converted where the browser can read it (Safari), rejected elsewhere;
D4 other files rejected; D5 video later.

Phase 1 as built: the server accepts JPEG, PNG, GIF and WebP, detected from the bytes, and strips metadata itself (EXIF, XMP,
IPTC, comments, PNG text chunks, WebP EXIF/XMP chunks) whatever the client did. The browser converts AVIF and HEIC before
upload. GIFs are stored as uploaded so animation survives.

Phase 2 (audio) built: WebM, Ogg, MP4/M4A, MP3 and WAV, detected from the bytes (plain Matroska and QuickTime are
refused). The server strips ID3 tags from MP3 and text/ID3 chunks from WAV; WebM, Ogg and MP4 are stored as uploaded
because their tags sit inside structures that would need re-muxing (browser recordings carry none). The browser measures
the length (recordings often do not state it) and the server stores it as display-only. Media is served with byte ranges
(Safari needs them to play audio at all). Recorder: composer Record button, #voice, and Record on any card; up to 10
minutes; the microphone is released on stop, cancel, or when the board redraws. Player: play/pause, seek, time, speed
(1x, 1.5x, 2x), one clip at a time.

Not yet built: alt text editing, upload cancel, SVG, video, zip export, transcription.

## Goal

A card can hold pictures or audio next to its text. Files arrive by drag and drop, paste, a file picker, or recording in the
browser. Pictures show inside the card at the right size; audio gets a small player, and a card can record a voice note.

## Model

A media card is an ordinary note with attachments. Text, tags, search, Markdown, todos and reminders keep working; the card
kind is `media` when a note has attachments and no stronger kind (reminder, open todo) applies.

    attachments(
      id INTEGER PRIMARY KEY,
      owner TEXT NOT NULL,           -- same owner as the note; checked on every read
      note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
      position INTEGER NOT NULL,     -- display order within the card
      blob_key TEXT NOT NULL,        -- key in the BlobStore
      sha256 TEXT NOT NULL,
      mime TEXT NOT NULL,            -- detected from the bytes, never taken from the client
      bytes INTEGER NOT NULL,
      width INTEGER, height INTEGER, -- images and video
      duration_ms INTEGER,           -- audio and video
      name TEXT,                     -- original file name, display only
      created_at INTEGER NOT NULL)

File bytes live behind a `BlobStore` trait (put, get, delete), following the `NoteStore` and auth-provider pattern. The default
stores files in a folder next to the database; an S3 store can plug in for AWS. Blob keys are
`<owner-hash>/<sha256>`: identical files are stored once per user, never shared across users (cross-user deduplication would
let one user confirm another user holds a given file).

## Formats

| Kind | Accepted | Display |
| --- | --- | --- |
| Images | JPEG, PNG, GIF, WebP, AVIF | `<img>` in the card |
| Vector | SVG | `<img>` only (scripts never run in `<img>`); never inlined into the page |
| iPhone photos | HEIC | converted to JPEG in the browser where it can decode HEIC (Safari 17+); elsewhere rejected with "save as JPEG" (decision D3) |
| Audio | WebM/Opus, Ogg/Opus, MP4/AAC (M4A), MP3, WAV | minimal player |
| Video (optional, D5) | MP4 (H.264), WebM | `<video controls preload="metadata">` |
| Other files (D4) | anything else | rejected, or a download chip with name and size |

Type is detected from the first bytes (magic numbers) and must be on the allowlist. Size caps per file: images 20 MB, audio
25 MB (about 25 minutes of Opus voice), video 100 MB. A per-user storage quota is in the multi-user spec.

## Input

- **Drag and drop:** the composer is a drop target; dropping files there attaches them to the card being written. Each card is
  also a drop target that appends to that card (shows "Add to this card"). While a file is dragged over the window, valid
  targets get a dashed outline and a label; dropping anywhere else does nothing (the browser's default "open the file" is
  prevented).
- **Paste:** pasting an image (for example a screenshot) into the composer attaches it.
- **Picker:** an "Attach" button opens the file picker (`accept` set to the allowlist).
- **Record:** a microphone button in the composer and on audio cards. `#voice` in the composer starts the recorder, like
  `#todo` switches type today; `#photo` opens the picker.
- Uploads show progress per file and can be cancelled; a failed file shows its error and does not block the rest.
- Images are resized in the browser before upload to at most 2560 px on the long side and re-encoded, which also strips EXIF
  metadata (GPS location from phones). Decision D2: keep the original too, or not.

## Display

- **Sizing:** width and height are stored, so the card reserves the right space before the image loads (no layout jump).
  Images scale to the card's width, keep their aspect ratio, and never upscale past their own size. Height is capped (feed
  480 px, board 260 px); taller images are letterboxed with `object-fit: contain`.
- **Several images:** one image is full width; two sit side by side; three or more form a grid of square-cropped thumbnails
  (`object-fit: cover`) with "+N" on the last tile when there are more than four.
- **Full size:** clicking an image opens it in a viewer over the page (Esc or click to close, arrow keys between images).
- **Thumbnails:** the browser generates a small preview at upload (about 640 px) so the board loads previews, not full photos.
- **Audio player:** one row: play/pause button, a seek bar, elapsed and total time. Optional speed toggle (1x, 1.5x, 2x).
  Uses the native `<audio>` element underneath, styled to match the themes; keyboard: space toggles, arrows seek.
- **Recorder:** record button, then a running timer and a level meter, then stop. The take can be played back, discarded or
  attached. Records with `MediaRecorder` in whatever format the browser supports (`isTypeSupported`), storing the real type.
  Microphone access needs a secure origin: localhost counts; hosted Chuck must be HTTPS.
- **Alt text and captions:** each image can have alt text; the card's own text is the caption.

## Serving

`GET /api/media/:id` and `/api/media/:id/thumb`, after the normal auth check and an owner check (another user's id returns 404).
Responses send the stored type, `X-Content-Type-Options: nosniff`, `Content-Security-Policy: default-src 'none'; sandbox`,
`Cache-Control: private`, and `Content-Disposition: inline` (or `attachment` for non-media files). With the sandbox policy an
SVG opened directly in a tab cannot run script. On AWS, serving media from a separate origin adds another layer.

## Other effects

- **Export:** the Markdown export references attachments; a zip export includes them (later).
- **Search:** media is not searchable; alt text and card text are. Local voice transcription (Whisper behind a `Transcriber`
  trait) is a later phase, and audio never leaves the machine.
- **Delete:** deleting a card deletes its attachments; a blob is removed when no attachment of that user references it.

## Decisions needed

- **D1 storage:** files in a folder next to the database (recommended), or BLOBs inside SQLite (one file to back up, worse for
  large files).
- **D2 originals:** keep the original upload as well as the resized, metadata-stripped copy, or only the copy.
- **D3 HEIC outside Safari:** reject with a message (recommended), or add a WebAssembly HEIC decoder (a few MB, may need
  approval at work).
- **D4 other files:** reject, or attach as download-only chips.
- **D5 video:** include now, or later.

## Phases

1. Photos: `attachments` table, `BlobStore`, upload, drag and drop, paste, picker, sizing, grid, viewer, safe serving.
2. Audio: player and recorder.
3. SVG, HEIC handling, video (if wanted), zip export.
4. Optional local transcription.
