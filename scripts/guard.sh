#!/bin/sh
# The security-surface audit. Run locally before pushing; CI runs it on every
# PR. Each check pins a promise search makes to its users — a PR that moves one
# of these boundaries must change this script in the same diff, where the review
# can see it.
set -eu
cd "$(dirname "$0")/.."
fail=0

say() { printf '%s\n' "$*"; }
bad() { fail=1; say "FAIL: $*"; }

# The shipped Rust sources: everything under src, minus test modules.
# A `#[cfg(test)]` module is the last item in a file, so truncating at the first
# marker removes it. Without this, test fixtures trip boundaries meant for the
# shipped binary.
prod_sources() {
    find src -type f -name '*.rs'
}

# 1. Network surface. Search talks to the engines defined in settings and the
#    pages a caller asks it to fetch. Built-in preset hosts live in Rust source
#    and are audited with every other call site. Operator-configured HTTP
#    adapters use the SSRF guard; command engines are deliberately trusted host
#    programs, not sandboxed. Hosts named only in a comment (a doc example, an
#    injection illustration) are not call sites and are skipped.
allowed_hosts="index.crates.io crates.io static.crates.io api.mwmbl.org example.com example.invalid localhost 127.0.0.1 0.0.0.0 github.com"
found_hosts=$(
    for f in $(prod_sources); do
        awk '/^#\[cfg\(test\)\]/ { exit } /^[[:space:]]*(\/\/|\*)/ { next } { print }' "$f"
    done
    )
found_hosts=$(printf '%s\n' "$found_hosts" | grep -ohE 'https?://[A-Za-z0-9.:-]+' | sed -E 's#https?://##' | sort -u
)
for host in $found_hosts; do
    case " $allowed_hosts " in
        *" $host "*) ;;
        *) bad "network host $host is not in the allowed list" ;;
    esac
done

# 2. Shipped code denies explicit panic sites: no unwrap, expect, panic!, or
#    unreachable! outside tests, except where a proof comment on the same or
#    preceding line explains why runtime input cannot reach the site.
for f in $(prod_sources); do
    findings=$(awk -v file="$f" '
        /^#\[cfg\(test\)\]/ { exit }
        { lines[NR] = $0 }
        END {
            for (n = 1; n <= NR; n++) {
                text = lines[n]
                if (text ~ /unwrap\(\)|expect\(|panic!\(|unreachable!\(/) {
                    prev = lines[n-1]
                    prev2 = lines[n-2]
                    if (prev !~ /clippy::expect_used|clippy::unwrap_used|proof:/ &&
                        prev2 !~ /clippy::expect_used|clippy::unwrap_used|proof:/) {
                        printf "FAIL: %s:%d: explicit panic site without a proof comment\n", file, n
                    }
                }
            }
        }
    ' "$f")
    if [ -n "$findings" ]; then bad "$findings"; fi
done

# 3. The SSRF guard must exist and deny the metadata address. Removing or
#    weakening it is the one change this file exists to catch.
guard=src/core/fetch/guard.rs
[ -f "$guard" ] || bad "the SSRF guard file $guard is missing"
grep -q "169.254" "$guard" || bad "the SSRF guard no longer covers link-local metadata"
grep -q "fn is_public_ip" "$guard" || bad "the SSRF guard's is_public_ip is gone"
grep -q "metadata.google.internal" "$guard" || bad "the SSRF guard no longer refuses the metadata hostname"

# 4. The fetcher must call the guard before dialing.
grep -q "guard::check_host" src/core/fetch/mod.rs || bad "the fetcher no longer calls the SSRF guard"

# Custom HTTP destinations are operator-configured but still guarded by default.
# Commands are trusted host code; direct children must die when a query is cancelled.
adapters=src/core/engines/adapter.rs
[ -f "$adapters" ] || bad "custom engine adapter implementation is missing"
grep -q 'check_host' "$adapters" || bad "custom HTTP adapters no longer check destination hosts"
grep -q 'GuardedResolver' "$adapters" || bad "custom HTTP adapters no longer guard DNS at dial time"
grep -q 'Policy::none()' "$adapters" || bad "custom HTTP adapters may follow redirects"
grep -q 'no_proxy()' "$adapters" || bad "custom HTTP adapters may use environment proxies"
grep -q 'kill_on_drop(true)' "$adapters" || bad "cancelled executable adapters may keep running"
grep -q 'env_clear()' "$adapters" || bad "executable engines inherit undeclared host credentials"
processors=src/client/processing.rs
[ -f "$processors" ] || bad "optional packet processor implementation is missing"
grep -q 'kill_on_drop(true)' "$processors" || bad "cancelled packet processors may keep running"
grep -q 'env_clear()' "$processors" || bad "packet processors inherit undeclared host credentials"
grep -q 'prepare_command_scratch' "$processors" || bad "packet processors skip private scratch validation"
[ ! -d src/core/engines/mwmbl ] && [ ! -d src/core/engines/searxng ] || bad "built-in engines must be adapter presets, not special implementations"

# 5. Paired HTTPS must wrap execution routes, including MCP. Pairing is the
#    only public admission; device hashes are reloaded on every request.
http=src/cli/http.rs
trust=src/cli/auth.rs
client=src/client/mod.rs
grep -q 'from_tcp_rustls' "$http" || bad "HTTPS listener is missing"
grep -q 'middleware::from_fn_with_state' "$http" || bad "device auth layer is not mounted"
grep -q 'host.authorized(presented)' "$http" || bad "request auth no longer checks current devices"
grep -q 'nest_service("/mcp", mcp)' "$http" || bad "MCP is not in the protected router"
grep -q 'read::<Devices>' "$trust" || bad "device hashes are not reloaded per admission"
grep -q 'ct_eq' "$trust" || bad "device/code comparison is not constant time"
grep -q 'pairing.attempts >= 20' "$trust" || bad "pairing attempt bound is missing"
grep -q 'now >= pairing.expires' "$trust" || bad "pairing expiry check is missing"
grep -q 'pairing.used = true' "$trust" || bad "pairing codes are not consumed"
grep -q 'mode(0o600)' "$trust" || bad "trust files are not owner-only"
grep -q 'builder.mode(0o700)' "$trust" || bad "trust dir is not owner-only"
grep -q 'lock_exclusive' "$trust" || bad "trust writers are not serialized"

# Windows needs equivalent guarantees, not an unconditional non-Unix bypass.
windows=src/private_fs.rs
[ -f "$windows" ] || bad "Windows private storage implementation is missing"
for primitive in SE_DACL_PROTECTED FILE_FLAG_OPEN_REPARSE_POINT FILE_ATTRIBUTE_REPARSE_POINT GetSecurityInfo; do
    grep -q "$primitive" "$windows" || bad "Windows private storage lacks $primitive"
done

grep -q 'update(&self.path.join("pairing.json")' "$trust" || bad "pair admissions do not lock persisted state"
grep -q 'update(&path.join("pairing.json")' "$trust" || bad "code renewal does not share the admission lock"
grep -q 'hash: hash(&code)' "$trust" || bad "pairing code is not stored as a hash"
# 6. A pin augments standard verification; it never replaces name, validity or
#    signature checks. Credential clients forbid plaintext, redirects and proxies.
grep -q 'fingerprint(&remote.cert)? != remote.fingerprint' "$client" || bad "pre-credential fingerprint check is missing"
grep -q 'cert != &self.cert' "$client" || bad "TLS leaf pin is missing"
for method in verify_server_cert verify_tls12_signature verify_tls13_signature; do
    tr -d '[:space:]' < "$client" | grep -q "self.verifier.$method" || bad "standard TLS $method delegation is missing"
done
grep -q 'WebPkiServerVerifier::builder_with_provider' "$client" || bad "standard WebPKI verifier is missing"
grep -q 'https_only(true)' "$client" || bad "credential client allows plaintext"
grep -q 'redirect(reqwest::redirect::Policy::none())' "$client" || bad "credential client follows redirects"
grep -q 'no_proxy()' "$client" || bad "credential client may use environment proxies"
if grep -rE 'danger_accept_invalid|ServerCertVerified::assertion|HandshakeSignatureValid::assertion|resolved_token|AUTH_TOKEN|DEFAULT_TOKEN_ENV' src; then
    bad "insecure TLS acceptance or legacy shared authentication is present"
fi

# Behavioral counterexamples complement these source checks in ./x check.
[ -f tests/cli/remote.rs ] || bad "offline paired-routing contract is missing"
grep -q 'pairing_bounds_and_hash_storage' "$trust" || bad "pairing bounds counterexamples are missing"

# 7. Modules replace crate boundaries, not dependency direction. Core, including
#    its engines, must stay independent of the CLI and protocol-facing adapters.
if grep -rE 'crate::(cli|mcp|client)(::|[;{ ])' src/core; then
    bad "core depends on a frontend or protocol adapter"
fi

if [ "$fail" -eq 0 ]; then
    say "guard: ok"
fi
exit "$fail"
