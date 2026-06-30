# List available recipes.
default:
    @just --list

# Bootstrap a fresh checkout: vendored schemas + crate dependencies.
init: vendor-init
    cargo fetch

# Build the workspace.
build:
    cargo build -p openapi-bindgen --target wasm32-wasip2 --release
    @echo "component: target/wasm32-wasip2/release/openapi_bindgen.wasm"

# `gen` regenerates bindings for a curated ~top-100 set of API providers.
# Picks one representative OpenAPI 3 spec per provider (the first `openapi.yaml`
# in sorted order) and writes WIT + Rust bindings to `components/<name>`.
# Providers that ship only Swagger 2.0 (`swagger.yaml`) or are absent from the
# vendored corpus are skipped; specs the generator can't yet handle are reported
# and skipped without aborting the run. A summary is printed at the end.

# Generate bindings for a curated set of ~top-100 API providers.
gen:
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
        docusign.net xero.com zuora.com qualtrics.com braze.com
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
        spec=$(find "vendor/schemas/APIs/$provider" \( -name openapi.yaml -o -name openapi.json \) 2>/dev/null | sort | head -n1)
        name="${provider%%.*}"
        if [[ -z "$spec" ]]; then
            printf 'skip  %-24s no OpenAPI 3 spec\n' "$provider"
            skip=$((skip + 1))
            continue
        fi
        if msg=$("$bin" "$spec" "components/$name" "autostamp:$name@0.1.0" 2>&1); then
            printf 'ok    %-24s components/%s\n' "$provider" "$name"
            ok=$((ok + 1))
        else
            reason=$(printf '%s' "$msg" | grep -v 'note: run with' | sed '/^[[:space:]]*$/d' | tail -n1 | sed 's/^[[:space:]]*//')
            printf 'FAIL  %-24s %s\n' "$provider" "$reason"
            fail=$((fail + 1))
        fi
    done
    printf '\ngenerated %d, failed %d, skipped %d (of %d providers)\n' "$ok" "$fail" "$skip" "${#providers[@]}"

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
