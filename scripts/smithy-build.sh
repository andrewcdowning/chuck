#!/usr/bin/env sh
# Builds the Smithy model in smithy/ and copies the generated OpenAPI document to docs/openapi/chuck.openapi.json.
# Needs the Smithy CLI (https://smithy.io/2.0/guides/smithy-cli/cli_installation.html) on PATH.
set -eu
cd "$(dirname "$0")/../smithy"
command -v smithy >/dev/null 2>&1 || { echo "smithy CLI not found on PATH; see scripts/smithy-build.sh" >&2; exit 1; }
smithy build
cp build/smithy/openapi/openapi/Chuck.openapi.json ../docs/openapi/chuck.openapi.json
echo "wrote docs/openapi/chuck.openapi.json"
