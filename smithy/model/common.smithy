$version: "2"

namespace chuck.api

// Shapes used by more than one service: the error bodies (always `{"error": "<message>"}`), timestamps (unix seconds)
// and the account role and status.

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

/// The data changed meanwhile (for example the card was removed while saving), or the account is in a state that forbids the
/// action. Look again and retry.
@error("client")
@httpError(409)
structure Conflict {
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

/// Unix seconds.
long Timestamp

list StringList {
    member: String
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
