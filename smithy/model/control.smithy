$version: "2"

namespace chuck.api

use aws.protocols#restJson1

/// The control plane, as a user sees it: their own account. Reading it, exporting everything they own, and deleting it.
/// Deleting is a command: the account is marked `deleting` (refused everywhere), its data is erased, then the account is
/// removed.
@restJson1
service ChuckControl {
    version: "2025-01-01"
    operations: [
        GetMe
        ExportMe
        DeleteMe
    ]
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
