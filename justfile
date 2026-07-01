# List available recipes.
default:
    @just --list

# Bootstrap a fresh checkout: vendored schemas + crate dependencies.
init: vendor-init
    cargo fetch

# Regenerate, build the generator component, then all generated component crates (or one: `just build nasa`).
build name="": (gen name)
    #!/usr/bin/env bash
    set -uo pipefail
    command -v component >/dev/null 2>&1 || { echo "the 'component' CLI is required: cargo install --git https://github.com/yoshuawuyts/component-registry component"; exit 1; }
    rustup target list --installed 2>/dev/null | grep -q wasm32-wasip2 || { echo "missing target: rustup target add wasm32-wasip2"; exit 1; }
    # gen already regenerated the component crates (via the recipe dependency).
    # Build the generator itself as a component in the default target dir.
    cargo build -p openapi-bindgen --target wasm32-wasip2 --release || exit 1
    echo "component: target/wasm32-wasip2/release/openapi_bindgen.wasm"
    # Build the generated component crates. A shared target dir lets the common deps
    # (wit-bindgen, serde_json, ...) compile once across all ~100 crates instead of once per crate.
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target/components}"
    ok=0; fail=0
    for dir in components/*/; do
        name="$(basename "$dir")"
        [[ -n "{{name}}" && "$name" != "{{name}}" ]] && continue
        [[ -f "$dir/wasm.toml" ]] || continue
        # The cargo crate (and thus the `.wasm` filename) is the package name from Cargo.toml,
        # which can differ from the directory name when the generator renames a keyword package
        # (e.g. dir `box` -> crate `box-api`). Derive both from Cargo.toml so they always agree
        # with the manifest's `[package].file = build/<crate>.wasm`.
        crate="$(sed -n 's/^name = "\(.*\)"/\1/p' "$dir/Cargo.toml" | head -n1)"
        crate="${crate:-$name}"
        artifact="${crate//-/_}.wasm"
        if (
            cd "$dir" || exit 1
            component install || exit 1
            # wit-bindgen reads wit/deps/; `component install` vendors to vendor/wit/ — bridge them.
            mkdir -p wit/deps
            cp vendor/wit/*.wit wit/deps/ || exit 1
            cargo build --quiet --target wasm32-wasip2 --release || exit 1
            mkdir -p build
            cp "$CARGO_TARGET_DIR/wasm32-wasip2/release/$artifact" "build/$crate.wasm" || exit 1
        ); then
            printf 'ok    %-24s %sbuild/%s.wasm\n' "$name" "$dir" "$crate"
            ok=$((ok + 1))
        else
            printf 'FAIL  %-24s\n' "$name"
            fail=$((fail + 1))
        fi
    done
    printf '\nbuilt %d, failed %d\n' "$ok" "$fail"
    [[ $fail -eq 0 ]]

# `gen` regenerates bindings for a curated ~top-100 set of API providers.
# Picks one representative spec per provider, preferring an OpenAPI 3 document
# (`openapi.yaml`/`openapi.json`) and falling back to Swagger 2.0
# (`swagger.yaml`/`swagger.json`); Swagger 2.0 is normalized to OpenAPI 3
# internally. Providers absent from the vendored corpus are skipped, as are
# specs the generator can't yet handle (reported and skipped without aborting
# the run). A summary is printed at the end.

# Generate bindings for a curated set of ~top-100 API providers (or just one: `just gen nasa`).
gen name="":
    #!/usr/bin/env bash
    set -uo pipefail
    cargo build --quiet -p openapi-bindgen --example run || { echo "build failed"; exit 1; }
    bin="target/debug/examples/run"
    providers=(
        # Tier 1 — foundational infrastructure
        amazonaws.com googleapis.com azure.com stripe.com twilio.com
        github.com openai.com slack.com google.com salesforce.com
        # Tier 2 — major platforms
        sendgrid.com mailchimp.com squareup.com adyen.com mastercard.com
        plaid.com gitlab.com atlassian.com bitbucket.org zoom.us
        digitalocean.com linode.com docker.com kubernetes.io spotify.com
        shutterstock.com vimeo.com soundcloud.com twitter.com instagram.com
        telegram.org ebay.com walmart.com adobe.com apple.com
        # Tier 3 — widely-integrated SaaS
        microsoft.com box.com hubapi.com notion.com asana.com
        trello.com zapier.com nytimes.com nasa.gov stackexchange.com
        giphy.com wikimedia.org here.com tomtom.com vonage.com
        # docusign.net intentionally excluded: its ~400-operation spec generates a
        # ~90k-line crate that exhausts rustc's memory on a 16 GB machine (see README).
        xero.com zuora.com qualtrics.com braze.com
        mandrillapp.com postmarkapp.com klarna.com bunq.com nordigen.com
        telnyx.com clicksend.com bulksms.com wolframalpha.com spoonacular.com
        # Tier 4 — strong niche & developer favorites
        rawg.io bungie.net trakt.tv musixmatch.com thetvdb.com
        tvmaze.com omdbapi.com polygon.io openfigi.com interactivebrokers.com
        weatherbit.io visualcrossing.com stormglass.io opencagedata.com graphhopper.com
        ipinfodb.com ip2location.com circleci.com snyk.io launchdarkly.com
        netlify.com vercel.com pinecone.io meilisearch.com elevenlabs.io
        languagetool.org getpostman.com tfl.gov.uk deutschebahn.com lufthansa.com
        amadeus.com ticketmaster.com medium.com dev.to magento.com
    )
    ok=0; fail=0; skip=0
    for provider in "${providers[@]}"; do
        name="${provider%%.*}"
        # When a name is given (e.g. `just gen nasa` / `just build nasa`), regenerate only it.
        [[ -n "{{name}}" && "$name" != "{{name}}" ]] && continue
        spec=$(find "vendor/schemas/APIs/$provider" \( -name openapi.yaml -o -name openapi.json \) 2>/dev/null | sort | head -n1)
        if [[ -z "$spec" ]]; then
            spec=$(find "vendor/schemas/APIs/$provider" \( -name swagger.yaml -o -name swagger.json \) 2>/dev/null | sort | head -n1)
        fi
        if [[ -z "$spec" ]]; then
            printf 'skip  %-24s no OpenAPI spec\n' "$provider"
            skip=$((skip + 1))
            continue
        fi
        msg=$("$bin" "$spec" "components/$name" "autostamp:$name@0.1.0" 2>&1)
        rc=$?
        if [[ $rc -eq 0 ]]; then
            printf 'ok    %-24s components/%s\n' "$provider" "$name"
            ok=$((ok + 1))
        elif [[ $rc -eq 3 ]]; then
            reason=$(printf '%s' "$msg" | sed -n 's/^skip: //p' | tail -n1)
            printf 'skip  %-24s %s\n' "$provider" "${reason:-no operations}"
            skip=$((skip + 1))
        else
            reason=$(printf '%s' "$msg" | awk '/^Caused by:/ {c=1; next} c && NF {sub(/^[[:space:]]+/, ""); print; exit}')
            if [[ -z "$reason" ]]; then
                reason=$(printf '%s' "$msg" | grep -v 'note: run with' | sed '/^[[:space:]]*$/d' | tail -n1 | sed 's/^[[:space:]]*//')
            fi
            printf 'FAIL  %-24s %s\n' "$provider" "$reason"
            fail=$((fail + 1))
        fi
    done
    printf '\ngenerated %d, failed %d, skipped %d (of %d providers)\n' "$ok" "$fail" "$skip" "${#providers[@]}"

# Publish built components to ghcr.io/autostamp via the `component` CLI. Pass a name for
# one; set `dry_run=1` to preview (`just publish-components "" 1`). Needs GHCR auth:
# GHCR_TOKEN/GH_TOKEN_CLASSIC (classic PAT, write:packages) or a prior `docker login ghcr.io`.
publish-components name="" dry_run="":
    #!/usr/bin/env bash
    set -uo pipefail
    command -v component >/dev/null 2>&1 || { echo "the 'component' CLI is required: cargo install --git https://github.com/yoshuawuyts/component-registry component"; exit 1; }
    tok="${GHCR_TOKEN:-${GH_TOKEN_CLASSIC:-}}"
    if [[ -n "$tok" ]]; then
        echo "$tok" | docker login ghcr.io -u token --password-stdin >/dev/null 2>&1 || { echo "docker login ghcr.io failed"; exit 1; }
    fi
    flag=""; [[ -n "{{dry_run}}" ]] && flag="--dry-run"
    ok=0; fail=0; skip=0
    for dir in components/*/; do
        name="$(basename "$dir")"
        [[ -n "{{name}}" && "$name" != "{{name}}" ]] && continue
        [[ -f "$dir/wasm.toml" ]] || continue
        # The artifact is named after the cargo crate, which may differ from the directory when
        # a keyword package was renamed (dir `box` -> crate `box-api`); `component publish` reads
        # the path from `[package].file`, so only this existence pre-check needs the crate name.
        crate="$(sed -n 's/^name = "\(.*\)"/\1/p' "$dir/Cargo.toml" | head -n1)"
        crate="${crate:-$name}"
        if [[ ! -f "$dir/build/$crate.wasm" ]]; then
            printf 'skip  %-24s no build/%s.wasm (run `just build %s` first)\n' "$name" "$crate" "$name"
            skip=$((skip + 1)); continue
        fi
        if component publish --manifest-path "$dir" $flag; then
            printf 'ok    %-24s\n' "$name"
            ok=$((ok + 1))
        else
            printf 'FAIL  %-24s\n' "$name"
            fail=$((fail + 1))
        fi
    done
    printf '\npublished %d, failed %d, skipped %d\n' "$ok" "$fail" "$skip"
    [[ $fail -eq 0 ]]

# Format-check, lint, and run the test suite.
test:
    cargo fmt --all -- --check
    cargo clippy --all --bins --examples -- -D warnings
    cargo test --all

# Fetch the vendored OpenAPI schemas submodule (sparse: only APIs/, shallow + blobless).
vendor-init:
    git submodule update --init --filter=blob:none vendor/schemas
    git -C vendor/schemas sparse-checkout init --cone
    git -C vendor/schemas sparse-checkout set APIs

# Update the vendored schemas to the latest upstream commit (re-pin to origin/main).
vendor-update:
    git -C vendor/schemas fetch --depth 1 origin main
    git -C vendor/schemas checkout -B main FETCH_HEAD
    @echo "vendor/schemas now at $(git -C vendor/schemas rev-parse --short HEAD) — run 'git add vendor/schemas' to stage the new pin."
