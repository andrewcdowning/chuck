$version: "2"

namespace chuck.api

use aws.protocols#restJson1

/// The Chuck HTTP API. Every operation requires a signed-in user; how the user signs in (local, shared token, proxy
/// header, JWT) is decided by the deployment, so no authentication scheme is modelled here. Each user sees only their
/// own data, administrators included. Error bodies are always `{"error": "<message>"}`.
@restJson1
service Chuck {
    version: "2025-01-01"
    operations: [
        ListNotes
        CreateNote
        UpdateNote
        DeleteNote
        MoveNote
        ExportNotes
        ListTags
        Suggest
        SuggestionFeedback
        ListRules
        AddRule
        DeleteRule
        ApplyRules
        AddAttachment
        GetMedia
        DeleteMedia
        PutThumb
        GetMe
        DeleteMe
        ExportMe
        ListUsers
        SetUserStatus
        DeleteUser
        SweepOrphans
    ]
}

// ---------- errors ----------

@error("client")
@httpError(400)
structure BadRequest {
    @required
    error: String
}

@error("client")
@httpError(401)
structure Unauthorized {
    @required
    error: String
}

@error("client")
@httpError(403)
structure Forbidden {
    @required
    error: String
}

@error("client")
@httpError(404)
structure NotFound {
    @required
    error: String
}

@error("client")
@httpError(413)
structure TooLarge {
    @required
    error: String
}

@error("server")
@httpError(500)
structure InternalError {
    @required
    error: String
}

// ---------- shapes ----------

/// Unix seconds.
long Timestamp

long NoteId

long AttachmentId

long RuleId

enum CardKind {
    NOTE = "note"
    TODO = "todo"
    REMINDER = "reminder"
    MEDIA = "media"
}

enum Repeat {
    DAILY = "daily"
    WEEKDAYS = "weekdays"
    WEEKLY = "weekly"
    MONTHLY = "monthly"
}

list StringList {
    member: String
}

structure Attachment {
    @required
    id: AttachmentId

    @required
    note_id: NoteId

    /// Detected from the file's bytes.
    @required
    mime: String

    @required
    bytes: Long

    width: Long
    height: Long

    /// Audio length as measured by the uploading browser (display only).
    duration_ms: Long

    /// Original file name (display only).
    name: String

    @required
    has_thumb: Boolean

    /// Changes whenever the file or its preview changes; put it in the media URL so caches never show a stale file.
    @required
    v: String
}

list AttachmentList {
    member: Attachment
}

structure Note {
    @required
    id: NoteId

    /// Derived from content: reminder, todo (has checklist items), media (has attachments) or note.
    @required
    kind: CardKind

    @required
    body: String

    @required
    tags: StringList

    @required
    created_at: Timestamp

    @required
    updated_at: Timestamp

    /// Next time the reminder is due.
    remind_at: Timestamp

    repeat: Repeat

    @required
    reminder_done: Boolean

    /// Due date of a todo card.
    due_at: Timestamp

    @required
    attachments: AttachmentList
}

list NoteList {
    member: Note
}

structure NoteInput {
    @required
    body: String

    remind_at: Timestamp
    repeat: Repeat
    reminder_done: Boolean
    due_at: Timestamp
}

structure TagCount {
    @required
    tag: String

    @required
    count: Long
}

list TagCountList {
    member: TagCount
}

structure Suggestion {
    @required
    tag: String

    /// Estimated probability (0 to 1) that the tag fits.
    @required
    confidence: Double

    /// "high" or "medium".
    @required
    tier: String

    /// The strongest evidence, for display.
    @required
    why: StringList
}

list SuggestionList {
    member: Suggestion
}

structure Rule {
    @required
    id: RuleId

    /// Keywords separated by `|` or `,`; the rule fires if any appears (case-insensitive).
    @required
    pattern: String

    @required
    tag: String
}

list RuleList {
    member: Rule
}

enum Role {
    USER = "user"
    ADMIN = "admin"
}

enum AccountStatus {
    ACTIVE = "active"
    DISABLED = "disabled"

    /// Being erased after a delete request; refused on every endpoint. Deleting it again finishes the job.
    DELETING = "deleting"
}

structure User {
    @required
    id: String

    display_name: String

    @required
    role: Role

    @required
    status: AccountStatus

    @required
    created_at: Timestamp

    last_seen_at: Timestamp
}

/// What an administrator sees about an account: counts and sizes, never content.
structure UserSummary {
    @required
    id: String

    display_name: String

    @required
    role: Role

    @required
    status: AccountStatus

    @required
    created_at: Timestamp

    last_seen_at: Timestamp

    @required
    notes: Long

    @required
    attachments: Long

    /// Total size of the account's stored files (previews not counted).
    @required
    storage_bytes: Long
}

list UserSummaryList {
    member: UserSummary
}

// ---------- notes ----------

/// Newest first; open todos come first. Supports full-text search, a tag, a date range and paging.
@readonly
@http(method: "GET", uri: "/api/notes")
operation ListNotes {
    input := {
        /// Free-text search (prefix match on each word).
        @httpQuery("q")
        q: String

        @httpQuery("tag")
        tag: String

        /// Inclusive range on created_at.
        @httpQuery("from")
        from: Timestamp

        @httpQuery("to")
        to: Timestamp

        /// Only pending reminders, soonest first.
        @httpQuery("reminders")
        reminders: Boolean

        @httpQuery("limit")
        limit: Integer

        @httpQuery("offset")
        offset: Integer
    }
    output := {
        @required
        notes: NoteList
    }
    errors: [Unauthorized, InternalError]
}

@http(method: "POST", uri: "/api/notes", code: 201)
operation CreateNote {
    input := {
        /// Set when files will be uploaded next, so the card may start without text.
        @httpQuery("media")
        media: Boolean

        @httpPayload
        @required
        note: NoteInput
    }
    output := {
        @httpPayload
        @required
        note: Note
    }
    errors: [BadRequest, Unauthorized, InternalError]
}

@idempotent
@http(method: "PUT", uri: "/api/notes/{id}")
operation UpdateNote {
    input := {
        @httpLabel
        @required
        id: NoteId

        @httpPayload
        @required
        note: NoteInput
    }
    output := {
        @httpPayload
        @required
        note: Note
    }
    errors: [BadRequest, Unauthorized, NotFound, InternalError]
}

@idempotent
@http(method: "DELETE", uri: "/api/notes/{id}", code: 204)
operation DeleteNote {
    input := {
        @httpLabel
        @required
        id: NoteId
    }
    output := {}
    errors: [Unauthorized, NotFound, InternalError]
}

/// Puts a card between two neighbours. At least one of prev and next is required. Open todos stay above other cards.
@http(method: "POST", uri: "/api/notes/{id}/move")
operation MoveNote {
    input := {
        @httpLabel
        @required
        id: NoteId

        /// The card that will be directly above this one, or absent for the top of its group.
        prev: NoteId

        /// The card that will be directly below this one, or absent for the bottom.
        next: NoteId
    }
    output := {
        @httpPayload
        @required
        note: Note
    }
    errors: [BadRequest, Unauthorized, NotFound, InternalError]
}

/// All matching notes as one Markdown file, oldest first. Takes the same filters as ListNotes; paging is ignored.
@readonly
@http(method: "GET", uri: "/api/export")
operation ExportNotes {
    input := {
        @httpQuery("q")
        q: String

        @httpQuery("tag")
        tag: String

        @httpQuery("from")
        from: Timestamp

        @httpQuery("to")
        to: Timestamp

        @httpQuery("reminders")
        reminders: Boolean
    }
    output := {
        @httpHeader("Content-Disposition")
        contentDisposition: String

        @httpPayload
        @required
        markdown: MarkdownBlob
    }
    errors: [Unauthorized, InternalError]
}

@mediaType("text/markdown")
blob MarkdownBlob

@readonly
@http(method: "GET", uri: "/api/tags")
operation ListTags {
    output := {
        @required
        tags: TagCountList
    }
    errors: [Unauthorized, InternalError]
}

// ---------- tag suggestions and rules ----------

structure SuggestionSet {
    @required
    suggestions: SuggestionList

    /// Notes with at least one tag the user wrote: what the model learned from.
    @required
    tagged_notes: Integer

    @required
    calibrated: Boolean

    /// Tagged notes needed before calibration.
    @required
    calibrate_at: Integer
}

/// Suggests tags for text being written, from the caller's own tagged notes.
@http(method: "POST", uri: "/api/suggest")
operation Suggest {
    input := {
        @required
        text: String
    }
    output := {
        @httpPayload
        @required
        result: SuggestionSet
    }
    errors: [Unauthorized, InternalError]
}

enum FeedbackAction {
    ACCEPTED = "accepted"
    DISMISSED = "dismissed"
}

@http(method: "POST", uri: "/api/suggest/feedback", code: 204)
operation SuggestionFeedback {
    input := {
        @required
        tag: String

        @required
        action: FeedbackAction

        /// The confidence that was shown for the suggestion.
        confidence: Double
    }
    output := {}
    errors: [BadRequest, Unauthorized, InternalError]
}

@readonly
@http(method: "GET", uri: "/api/rules")
operation ListRules {
    output := {
        @required
        rules: RuleList
    }
    errors: [Unauthorized, InternalError]
}

@http(method: "POST", uri: "/api/rules", code: 201)
operation AddRule {
    input := {
        @required
        pattern: String

        @required
        tag: String
    }
    output := {
        @httpPayload
        @required
        rule: Rule
    }
    errors: [BadRequest, Unauthorized, InternalError]
}

@idempotent
@http(method: "DELETE", uri: "/api/rules/{id}", code: 204)
operation DeleteRule {
    input := {
        @httpLabel
        @required
        id: RuleId
    }
    output := {}
    errors: [Unauthorized, NotFound, InternalError]
}

/// Applies every rule to the caller's existing notes.
@http(method: "POST", uri: "/api/rules/apply")
operation ApplyRules {
    output := {
        /// Number of notes whose tags changed.
        @required
        changed: Integer
    }
    errors: [Unauthorized, InternalError]
}

// ---------- files ----------

@mediaType("application/octet-stream")
blob FileBytes

@mediaType("application/octet-stream")
@streaming
blob FileStream

/// The body is the raw file (image, or WebM, Ogg, M4A, MP3 or WAV audio). The type is detected from the bytes and
/// location metadata is stripped before storing.
@http(method: "POST", uri: "/api/notes/{id}/attachments", code: 201)
operation AddAttachment {
    input := {
        @httpLabel
        @required
        id: NoteId

        /// Original file name (display only).
        @httpQuery("name")
        name: String

        /// Audio length measured by the browser, 0 to 6 hours (display only).
        @httpQuery("duration_ms")
        duration_ms: Long

        @httpPayload
        @required
        file: FileBytes
    }
    output := {
        @httpPayload
        @required
        attachment: Attachment
    }
    errors: [BadRequest, Unauthorized, NotFound, TooLarge, InternalError]
}

/// Serves a file, or its preview with thumb=true. Supports Range (206) and conditional (304) requests. The response is
/// cacheable for a long time only when v matches the attachment's current version.
@readonly
@http(method: "GET", uri: "/api/media/{id}")
operation GetMedia {
    input := {
        @httpLabel
        @required
        id: AttachmentId

        @httpQuery("thumb")
        thumb: Boolean

        /// The attachment's version, from its JSON.
        @httpQuery("v")
        v: String

        @httpHeader("Range")
        range: String

        @httpHeader("If-None-Match")
        ifNoneMatch: String
    }
    output := {
        @httpHeader("Content-Type")
        contentType: String

        @httpHeader("Content-Range")
        contentRange: String

        @httpHeader("ETag")
        etag: String

        @httpHeader("Cache-Control")
        cacheControl: String

        @httpPayload
        @required
        file: FileStream
    }
    errors: [Unauthorized, NotFound, InternalError]
}

@idempotent
@http(method: "DELETE", uri: "/api/media/{id}", code: 204)
operation DeleteMedia {
    input := {
        @httpLabel
        @required
        id: AttachmentId
    }
    output := {}
    errors: [Unauthorized, NotFound, InternalError]
}

/// The body is a small preview image (at most 1024 pixels on each side) made by the client.
@idempotent
@http(method: "PUT", uri: "/api/media/{id}/thumb", code: 204)
operation PutThumb {
    input := {
        @httpLabel
        @required
        id: AttachmentId

        @httpPayload
        @required
        image: FileBytes
    }
    output := {}
    errors: [BadRequest, Unauthorized, NotFound, TooLarge, InternalError]
}

// ---------- the signed-in account ----------

@readonly
@http(method: "GET", uri: "/api/me")
operation GetMe {
    output := {
        @httpPayload
        @required
        user: User
    }
    errors: [Unauthorized, InternalError]
}

@mediaType("application/zip")
@streaming
blob ZipStream

/// Everything the caller owns as one ZIP: notes (JSON and Markdown), rules, suggestion feedback and original files.
/// Fails with 413 when the files total more than 1 GB.
@readonly
@http(method: "GET", uri: "/api/me/export")
operation ExportMe {
    output := {
        @httpHeader("Content-Disposition")
        contentDisposition: String

        @httpPayload
        @required
        archive: ZipStream
    }
    errors: [Unauthorized, TooLarge, InternalError]
}

/// Permanently erases the caller's account, notes, files and rules. Signing in again starts a new, empty account.
@suppress(["HttpMethodSemantics.UnexpectedPayload"])
@idempotent
@http(method: "DELETE", uri: "/api/me", code: 204)
operation DeleteMe {
    input := {
        /// Must equal the account id.
        @required
        confirm: String
    }
    output := {}
    errors: [BadRequest, Unauthorized, InternalError]
}

// ---------- administration (accounts only, never content) ----------

@readonly
@http(method: "GET", uri: "/api/admin/users")
operation ListUsers {
    output := {
        @required
        users: UserSummaryList
    }
    errors: [Unauthorized, Forbidden, InternalError]
}

/// Disables or enables an account. A disabled account is refused on every endpoint. Not allowed on your own account.
/// Ids are sent in the body because they may contain slashes.
@http(method: "POST", uri: "/api/admin/users/status")
operation SetUserStatus {
    input := {
        @required
        id: String

        @required
        active: Boolean
    }
    output := {
        @httpPayload
        @required
        user: User
    }
    errors: [BadRequest, Unauthorized, Forbidden, NotFound, InternalError]
}

/// Deletes another account and everything it owns. To keep someone out without deleting, disable the account.
@idempotent
@http(method: "POST", uri: "/api/admin/users/delete", code: 204)
operation DeleteUser {
    input := {
        @required
        id: String
    }
    output := {}
    errors: [BadRequest, Unauthorized, Forbidden, NotFound, InternalError]
}

/// Deletes stored files that no card refers to and that have not been written for ten minutes (files of deleted cards and
/// attachments, replaced previews, leftovers of interrupted uploads). Also runs on a schedule. Reports counts only.
@http(method: "POST", uri: "/api/admin/maintenance/sweep")
operation SweepOrphans {
    output := {
        /// Files looked at.
        @required
        scanned: Integer

        /// Unused files deleted.
        @required
        removed: Integer

        /// Unused files left alone because they were written too recently.
        @required
        too_new: Integer

        /// Files that could not be deleted; they are tried again next time.
        @required
        failed: Integer
    }
    errors: [Unauthorized, Forbidden, InternalError]
}
