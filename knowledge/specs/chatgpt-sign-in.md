---
type: Product Specification
title: ChatGPT sign-in
description: Defines the two routes Yolop's ChatGPT (codex provider) sign-in can take, the borrowed Codex client and OpenAI's open-source Sign in with ChatGPT, how each is configured, and how refresh stays bound to the issuing client.
---

# ChatGPT sign-in

Status: configurable Codex client implemented; the open-source route is the
default for new browser sign-ins since 2026-10-03 (owner's decision), verified
against local mocks only, not yet against a live ChatGPT account.
`chatgpt_sign_in = "codex"` (or `YOLOP_CHATGPT_SIGN_IN=codex`) selects the
Codex route.

## Why

The `codex` provider signs in with a ChatGPT account and runs turns on the
user's ChatGPT plan through the Codex backend. Yolop has no OAuth client of its
own for that, so it borrows the Codex CLI's public client ID. That works, but it
presents Yolop to OpenAI as the Codex CLI, and the ID is not Yolop's to keep.

OpenAI now offers "Sign in with ChatGPT" (SIWC) for third-party apps
(developers.openai.com/siwc). Yolop should be able to move to a client of its
own without a code change in every place that talks OAuth, and without
stranding the logins users already have.

## What

### One configured client for new sign-ins

A new sign-in (browser or device flow, from the TUI, `yolop setup login
codex`, or ACP) uses the first of:

1. `YOLOP_CHATGPT_CLIENT_ID`,
2. the `chatgpt_client_id` setting (`yolop config set chatgpt_client_id ...`),
3. the borrowed Codex CLI client, the built-in default.

A blank value counts as unset. A malformed one (whitespace, control or
non-ASCII characters, or over 256 bytes) is an error naming its source, never a
silent fall back to the borrowed client: a user who configured their own client
must not end up signed in with someone else's. `chatgpt_client_id` is
global-only, like the credentials it governs; a profile cannot set it.

### Refresh stays with the issuing client

A refresh token is bound to the client that issued it. Each saved token set
records that client (`client_id` under `[codex_auth]`), and refresh always uses
the recorded one, never whatever is configured now. Changing the setting
therefore affects only the next sign-in; an existing login keeps refreshing
until the user signs in again. A record saved before the field existed was
issued to the borrowed client, so that is its fallback. A pasted access token
has no refresh token and records no client.

### Switching the default

When OpenAI issues Yolop a client ID, the borrowed default in
`src/auth/codex.rs` becomes Yolop's own in one change. Existing logins keep
refreshing with the borrowed client they were issued to, so nobody is logged
out by the switch; they move over on their next sign-in.

## What SIWC actually offers (as read 2026-10-02)

The SIWC documentation (developers.openai.com/siwc, and its single-file export
at `/siwc/llms-full.txt`) describes two different things, and only one of them
is a client ID to request:

- **Sign-in on a website or in a ChatGPT plugin** is for commercial partners,
  through an interest form and a limited trial. This is the "request a client
  ID" path, and it is identity sign-in, not plan usage. The configurable
  client above covers it and any static ID OpenAI issues Yolop.
- **ChatGPT plan usage for open-source apps** needs no requested ID. The owner
  chose this route; the next section pins what Yolop implements of it.

## The open-source route

Selected per sign-in by `YOLOP_CHATGPT_SIGN_IN`, then the `chatgpt_sign_in`
setting (`codex` or `open-source`, global-only), then `open-source`. An unknown value
is an error naming its source. The route is recorded on the login it produces,
so changing the setting affects only the next sign-in, like the client ID.

### Sign-in

1. **Host ID.** On first use Yolop generates `ext_agent_host_id` as a
   `urn:uuid:` UUIDv4 and persists it as `chatgpt_host_id`; every later
   authorization from this host sends the same value. The docs recommend a JWK
   thumbprint URI but accept UUIDs; OpenAI does not verify key possession, so
   a key pair adds nothing yet.
2. **Authorize.** A loopback listener on `127.0.0.1:1455`, or any free port
   when that one is taken (the docs allow the port, and only the port, to
   vary), with redirect `http://127.0.0.1:<port>/auth/callback`. Fresh
   `state`, `nonce`, and PKCE (S256) per attempt. The system browser opens
   `https://auth.openai.com/api/accounts/authorize` with `response_type=code`,
   `scope=openid profile email offline_access resource.invoke
   chatgpt.tokens.use.direct`, `resource=https://api.openai.com/v1`, and
   `ext_agent_host_id`. A first sign-in sends `client_id=dynamic_agent_client`
   and `agent_name_hint=Yolop`; a returning one sends the saved issued client,
   no name hint, `login_hint` from the saved email, and `id_token_hint` when the
   current login was issued to that client. `prompt=consent` is added only when
   the saved login lacks the plan scope, the documented re-enable path.
3. **Callback.** The state must match. `error=access_denied` ends the attempt
   with nothing saved. A first sign-in must return an issued `client_id` that
   is not `dynamic_agent_client`; a returning one may omit it but must not
   change it, or the result is rejected.
4. **Exchange.** Form POST to `https://auth.openai.com/api/accounts/oauth/token`
   with the issued client, code, verifier, the same redirect URI, and the same
   resource. No secret.
5. **Validate.** The ID token is verified against
   `https://auth.openai.com/.well-known/jwks.json` (RS256, the only algorithm
   the discovery document lists), then issuer `https://auth.openai.com`,
   audience equal to the issued client, expiry (60 s skew), and the attempt's
   nonce. Its `sub` is the account identity. A returning sign-in whose `sub`
   differs from the saved registration is rejected without touching
   credentials.
6. **Store.** The registration (`[chatgpt_registration]`: client, subject,
   email) is saved as soon as identity is proven. The token set goes into
   `[codex_auth]` as usual, with `flow = "open-source"`, the issued
   `client_id`, the retained `id_token`, the granted `scope`, and the
   `subject`. Without `chatgpt.tokens.use.direct` in the granted scopes the
   login is saved, so the retry reuses the client and asks for consent, but
   sign-in reports that plan use was not granted.

The route is browser-only; the docs describe no device flow. A device sign-in
with no route configured uses the Codex client, since the open-source route is
only the default there. When `open-source` is chosen explicitly, a device
sign-in is an error rather than a silent switch to Codex. One registration per host: there is no account picker yet, so signing
in to a different account means removing `[chatgpt_registration]` first.

### Refresh and sign-out

Everruns' shared credential lifecycle refreshes with the client that issued the
saved grant. Yolop supplies private cross-process settings leases and atomic
compare-and-save operations. A stale refresh cannot restore an older login or
replace a newer registration. Rotating access and refresh tokens save together
before inference uses them; another route's disk record is never adopted.

Disconnect confirms plan-grant revocation before clearing local credentials.
A failed revocation retains the grant for retry. Registration and host ID remain
for the next sign-in.

### Inference

The `codex` provider name and existing browser/device UX stay in Yolop. A saved
`flow = "open-source"` selects Everruns' ChatGPT-plan driver on the public
Responses API; other logins select the shared legacy Codex driver. Everruns
owns request shaping, stream parsing, quota errors, token refresh and native
compaction behavior. The plan route sends stateless `store: false` requests,
requires the granted plan scope, and fails incomplete streams.

See [Everruns' ChatGPT and Codex drivers](https://github.com/everruns/everruns/tree/main/crates/drivers/drivers/src)
for the wire implementation and signed OAuth fixtures.

## Remaining work

A live browser consent and turn against a ChatGPT account remain unverified.
Account selection across several registrations and a ChatGPT-specific model
catalog are separate product work; this migration preserves Yolop's existing
single-registration experience and model menu.

## Ownership boundary

`src/auth/codex.rs` owns legacy browser/device UX, client configuration and route
selection. `src/auth/siwc.rs` adapts the shared open-source login to Yolop's
browser and settings. `src/drivers/codex.rs` registers shared drivers and provides
the leased token store. `src/config` owns the on-disk representation. Protocol
validation, transport and refresh sequencing live in `everruns-drivers`.

## Related

- [Configuration](configuration.md), the settings schema the key lives in.
- [Model list](model-list.md), the models the `codex` provider offers.

## Shared Everruns driver boundary

Everruns 0.38 owns both protocol drivers, OAuth validation, shared credential
shapes, and refresh sequencing in `everruns-drivers`. Yolop supplies settings
storage, installation identity, browser navigation, and route selection. The
settings adapter reads current disk credentials under a cross-process lease and
atomically compares before saving rotations, so a new login or logout cannot be
restored by an older refresh. Disconnect revokes a plan grant before clearing it;
a failed revocation retains it for retry. Legacy browser/device routes retain
their existing host UX while using Everruns for inference and refresh.

The migration uses exact, aligned registry versions from the published 0.38
batch. Shared provider and capability types come from `everruns-contracts`;
retired provider and capability facade crates are not dependencies. Local path
patches are not a distribution dependency.
