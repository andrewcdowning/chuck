# API contract

The HTTP API is described by a [Smithy](https://smithy.io) model in `smithy/model/` (protocol `restJson1`), split into
one service per plane (see [specs/service-layer.md](specs/service-layer.md)). The server is still hand-written axum code
in `crates/chuck-http/src/api.rs`; the model is the written contract, not a code generator.

| Service | Plane | Who calls it | Model file | Router function | OpenAPI document |
| --- | --- | --- | --- | --- | --- |
| `ChuckData` | Data: cards, tags, rules, files, tag suggestions | Signed-in users | `data.smithy` | `data_routes` | `docs/openapi/chuck-data.openapi.json` |
| `ChuckControl` | Control: the caller's own account (read, export, delete) | Signed-in users | `control.smithy` | `control_routes` | `docs/openapi/chuck-control.openapi.json` |
| `ChuckAdmin` | Management: accounts and maintenance | Administrators | `admin.smithy` | `admin_routes` | `docs/openapi/chuck-admin.openapi.json` |

`common.smithy` holds what the services share: the error bodies and the account role and status. Paths do not change
between planes, so clients are unaffected by the split.

- **Generated OpenAPI:** OpenAPI 3.0.2, one document per service. Regenerate them with `scripts/smithy-build.sh` after
  editing the model. The script needs the [Smithy CLI](https://smithy.io/2.0/guides/smithy-cli/cli_installation.html),
  which bundles its own Java runtime. The first build downloads the AWS traits and OpenAPI converter from Maven Central.
- **Keeping it honest:** `tests/contract.rs` (part of `cargo test`, no Smithy tools needed) fails when a service's
  operations differ from the routes of its router function, when an HTTP method differs, or when an operation is in no
  service or in two. It does not check request and response shapes.
- **Serving some planes only:** `api::router_for(state, &[Plane::Data])` builds a router with just that plane (the page and
  `/healthz` are always served). Other planes' paths are 404, and anonymous callers still get 401 first. `api::router`
  serves all three, as the server does. This is the seam for running one function per plane behind a gateway.
- **Authentication** is not in the model. It is chosen per deployment (see [AUTH.md](AUTH.md)), so the documents have no
  security scheme; add one for your gateway when you publish them.

## Ids

Cards, files (attachments) and rules are identified by 26-character text ids such as `01M8FN3JY0P5P1FP9T6X9D59KC`. Treat
them as opaque text: the server makes them, later ids sort after earlier ones, and nothing else is promised. A request for
anything that is not an id of that shape (for example an old numeric id from a bookmark) is a 404, like a card that does not
exist. Releases before this change used numbers; clients that stored those numbers must start again from a fresh listing.
Account ids are unchanged: they are whatever the sign-in provides.

## Errors

Error bodies are always `{"error": "<message>"}`. 400 means the request itself is wrong, 401 and 403 are about who you are,
404 means the thing is not yours or does not exist, 409 (`Conflict`) means the data changed while saving or the account is
in a state that forbids the action (for example being deleted), so look again and retry, 413 means too large, and 500 is a
fault of the server that is logged and not described.

## Known gaps in the model

List endpoints return objects (`{"notes": [...]}`, `{"tags": [...]}`, `{"rules": [...]}`, `{"users": [...]}`) because Smithy cannot bind a bare array to a body.

- `DELETE /api/me` takes a JSON body, which Smithy flags as unusual for DELETE. The warning is suppressed on purpose.
- `ChuckAdmin` refers to the `User` shape defined in `control.smithy` (the response of disabling an account), so its
  document also contains that schema.

## Toward API Gateway and Lambda

Each plane is a router of its own, and the services share a request edge (`require_principal`) that authenticates once and
hands handlers a `Principal`. What remains for a Lambda deployment is the package split (phase 7 of the service layer plan)
and a Lambda host that maps API Gateway events to these routers. Two things will need a decision then: uploads and
downloads over 10 MB (API Gateway's payload limit; use pre-signed S3 URLs) and the in-memory ZIP export (also limited by
Lambda memory and the 6 MB response limit).
