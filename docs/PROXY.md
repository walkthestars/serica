# Outbound proxies and rate limiting

Two operational topics that matter once Serica runs behind infrastructure rather than on a laptop: outbound proxies (opt-in) and what happens to rate limiting when a reverse proxy sits in front of Serica.

## Outbound proxy support

`SERICA_PROXY__SEARCH_URL` and `SERICA_PROXY__EXTRACT_URL` configure outbound proxies for the two kinds of upstream traffic. They're independent — you can set one without the other, and each defaults to unproxied. Both accept any URL `reqwest` understands as a proxy (`http://`, `https://`, or `socks5://`, with optional `user:pass@` credentials).

**`SERICA_PROXY__SEARCH_URL` is safe to set freely.** It only affects the DuckDuckGo search client, which has no IP-pinning guarantee for a proxy to interfere with.

**`SERICA_PROXY__EXTRACT_URL` weakens SSRF protection — read this before setting it.** `/api/v1/extract` and `serica_extract` fetch a caller-supplied URL. Before dialling it, Serica resolves the hostname, rejects it if it points at a private/loopback/link-local/cloud-metadata address, and then pins the HTTP client to connect to *exactly* that validated address — closing a DNS-rebinding window where the hostname could resolve to something else by the time the connection is actually made. The same check runs again, against a freshly pinned client, on every redirect hop.

A proxy defeats this: when proxied, the client doesn't resolve the hostname itself — it hands the hostname to the proxy in the CONNECT request, and the *proxy* resolves it. The address Serica validated and the address ultimately dialled can differ. Setting `SERICA_PROXY__EXTRACT_URL` makes private-range protection dependent entirely on the proxy's own egress policy instead of Serica's. Serica logs a prominent startup warning whenever this is set, but still honors it — don't set it unless the proxy itself enforces equivalent restrictions on where it will connect.

If neither variable is set, Serica also explicitly disables `HTTP_PROXY`/`HTTPS_PROXY`-style environment auto-detection for the extract client, so an unrelated system-wide proxy variable set for other tools can't silently weaken this protection without an operator opting in through `SERICA_PROXY__EXTRACT_URL` specifically.

## Rate limiting does not survive being fronted by a proxy

`SERICA_SERVER__RATE_LIMIT_PER_MINUTE` is enforced per **peer IP address** — the actual TCP connection's source address, via `tower_governor`'s `PeerIpKeyExtractor`. This works correctly for a self-hosted Serica reachable directly.

**It stops working correctly the moment anything sits in front of Serica** — a reverse proxy, load balancer, CDN, or API gateway. Every request Serica sees then arrives from the same peer address (the front-end's), so every caller behind it shares one rate-limit bucket: one abusive client can exhaust the budget for everyone else behind the same proxy, and the effective limit becomes "N requests/minute for the whole fleet," not "N requests/minute per client."

**There is currently no config flag to fix this**, and Serica does not read `X-Forwarded-For`/`X-Real-IP`/`Forwarded` for rate-limiting purposes — on purpose. Honoring one of those headers by default would be a trivial bypass: since the header is client-supplied unless a trusted proxy strips or overwrites it, any caller could set a fresh value on every request and get a fresh bucket every time, defeating the limiter entirely rather than merely weakening it.

Doing this safely isn't just a matter of switching key extractors — `tower_governor` ships a `SmartIpKeyExtractor` that reads `X-Forwarded-For`, but it unconditionally trusts the *first* comma-separated address in the header with no way to configure how many proxy hops are trusted. That's only correct if you know your exact proxy topology (single hop, and the proxy itself strips any client-supplied `X-Forwarded-For` before appending its own) — Serica has no such topology config today, so wiring in that extractor as-is would trade "always broken behind a proxy" for "silently spoofable behind a proxy," which is worse. Implementing this properly needs a trusted-hop-count (or trusted-proxy-CIDR) setting, which doesn't exist yet — this is deferred rather than half-built, and is an open TODO for a future release.

**If you front Serica with a reverse proxy today**, either rely on the proxy's own per-client rate limiting (most reverse proxies and gateways have one), or treat Serica's built-in limiter as a coarse fleet-wide backstop rather than a per-caller guarantee.
