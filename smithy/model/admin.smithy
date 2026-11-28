$version: "2"

namespace chuck.api

use aws.protocols#restJson1

/// The management plane: administrators manage accounts and run maintenance. Administrators see counts and sizes, never
/// anyone's content, and cannot disable or delete their own account here. Every operation is refused to non-administrators
/// with 403.
@restJson1
service ChuckAdmin {
    version: "2025-01-01"
    operations: [
        ListUsers
        SetUserStatus
        DeleteUser
        SweepOrphans
    ]
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
