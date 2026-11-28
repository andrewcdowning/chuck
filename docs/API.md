# API contract

The HTTP API is described by a [Smithy](https://smithy.io) model in `smithy/model/chuck.smithy` (protocol `restJson1`).
The server is still hand-written axum code in `src/api.rs`; the model is the written contract, not a code generator.

- **Generated OpenAPI:** `docs/openapi/chuck.openapi.json` (OpenAPI 3.0.2). Regenerate it with `scripts/smithy-build.sh`
  after editing the model. The script needs the [Smithy CLI](https://smithy.io/2.0/guides/smithy-cli/cli_installation.html),
  which bundles its own Java runtime. The first build downloads the AWS traits and OpenAPI converter from Maven Central.
- **Keeping it honest:** `tests/contract.rs` (part of `cargo test`, no Smithy tools needed) fails when a route or HTTP
  method differs between the router and the model. It does not check request and response shapes.
- **Authentication** is not in the model. It is chosen per deployment (see [AUTH.md](AUTH.md)), so the document has no
  security scheme; add one for your gateway when you publish it.

## Known gaps in the model

List endpoints return objects (`{"notes": [...]}`, `{"tags": [...]}`, `{"rules": [...]}`, `{"users": [...]}`) because Smithy cannot bind a bare array to a body.

- `DELETE /api/me` takes a JSON body, which Smithy flags as unusual for DELETE. The warning is suppressed on purpose.

## Toward API Gateway and Lambda

Splitting for Lambda would put the handlers behind a shared core crate (stores, auth, media) with one thin entry point per
host (axum server, Lambda). The route list in the model is what an API Gateway definition needs. Two things will need a
decision then: uploads and downloads over 10 MB (API Gateway's payload limit; use pre-signed S3 URLs) and the in-memory
ZIP export (also limited by Lambda memory and the 6 MB response limit).
