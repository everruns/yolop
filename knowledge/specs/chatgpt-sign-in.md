---
type: Product Specification
title: ChatGPT sign-in
description: Defines which OAuth client Yolop's ChatGPT (codex provider) sign-in uses, how it is configured, how refresh stays bound to the issuing client, and the open path to OpenAI's Sign in with ChatGPT.
---

# ChatGPT sign-in

Status: configurable client implemented; Yolop's own client not yet issued.

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

The SIWC documentation describes two different things, and only one of them is
a client ID to request:

- **Sign-in on a website or in a ChatGPT plugin** is for commercial partners,
  through an interest form and a limited trial. This is the "request a client
  ID" path, and it is identity sign-in, not plan usage.
- **ChatGPT plan usage for open-source apps** needs no requested ID. The app
  registers dynamically: the first authorization sends
  `client_id=dynamic_agent_client` with an `agent_name_hint` and a stable
  per-host `ext_agent_host_id`, and the callback returns a client ID issued to
  that user and workspace, which the app saves and reuses for later sign-ins and
  every refresh. It uses different endpoints (`/api/accounts/authorize`,
  `/api/accounts/oauth/token`), scopes (`offline_access resource.invoke
  chatgpt.tokens.use.direct`), and a `resource` of `https://api.openai.com/v1`.
  Inference goes to the public Responses API, not the Codex backend, with
  `store: false`, `stream: true`, and preview limits that rule out several
  request fields Yolop's drivers send today (for example `max_output_tokens`,
  `temperature`, `metadata`, `previous_response_id`, and hosted tools).

The configurable client covers the first path and any static ID OpenAI issues
Yolop. Adopting the open-source path is a separate piece of work: a persisted
host ID, per-account registrations keyed by issued client ID, ID-token
validation against OpenAI's JWKS, a granted-scope check, revocation on sign-out,
and a driver for the public Responses API within the preview limits. The
per-token `client_id` recorded here is the storage that path needs as well.

## Ownership boundary

`src/auth/codex.rs` owns client resolution, validation, and the OAuth flows.
`src/config` persists the setting and the issuing client. `src/drivers/codex.rs`
reads the issuing client at refresh time, so a record adopted from disk mid-run
refreshes with its own client.

## Related

- [Configuration](configuration.md), the settings schema the key lives in.
- [Model list](model-list.md), the models the `codex` provider offers.
