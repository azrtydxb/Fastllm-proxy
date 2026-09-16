# UI shows connection status

Status: closed
Created: 2026-09-16
Epic: chatgpt-oauth-provider
Sprint: 002-add-chatgpt-oauth-provider-credential-kind

## Description

The provider screen shows whether the ChatGPT OAuth connection is active.

## Acceptance criteria

- [x] UI shows connection status

## Evidence

`web/src/views/Providers.jsx` now:

- Fetches OAuth status on page load for all `chatgpt_oauth` providers (lines 95–114)
- Shows connection status per provider card with dot indicator: "OAuth connected · expires in Xmin" or "OAuth disconnected" (lines 695–729)
- Shows Connect button that opens PKCE challenge URL in a new window (lines 740–750)
- Shows Disconnect button when connected (lines 732–738)
- Shows connecting/disconnecting loading state (lines 730–731, 721–723)
- "Add provider" form includes `chatgpt_oauth` as a selectable credential kind (line 129)
- Shows hint text when `chatgpt_oauth` is selected in the form (lines 599–603)
