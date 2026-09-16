# Add ChatGPT OAuth provider credential kind

Status: active
Created: 2026-09-16

## Goal

A user can add a ChatGPT Pro subscription as a provider in FastLLM, connect
it via OAuth, and have its tokens auto-refreshed — routing through it and
falling over on OAuth errors the same as any other backend.
