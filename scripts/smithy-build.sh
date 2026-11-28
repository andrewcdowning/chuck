#!/usr/bin/env sh
# Builds the Smithy model in smithy/ and copies the generated OpenAPI documents, one per service (data, control and admin
# planes), to docs/openapi/. Needs the Smithy CLI (https://smithy.io/2.0/guides/smithy-cli/cli_installation.html) on PATH.
set -eu
cd "$(dirname "$0")/../smithy"
command -v smithy >/dev/null 2>&1 || { echo "smithy CLI not found on PATH; see scripts/smithy-build.sh" >&2; exit 1; }
smithy build
rm -f ../docs/openapi/*.openapi.json
cp build/smithy/data/openapi/ChuckData.openapi.json ../docs/openapi/chuck-data.openapi.json
cp build/smithy/control/openapi/ChuckControl.openapi.json ../docs/openapi/chuck-control.openapi.json
cp build/smithy/admin/openapi/ChuckAdmin.openapi.json ../docs/openapi/chuck-admin.openapi.json
echo "wrote docs/openapi/chuck-{data,control,admin}.openapi.json"
