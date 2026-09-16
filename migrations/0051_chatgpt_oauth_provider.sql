-- Add ChatGPT OAuth credential kind.
--
-- A new `chatgpt_oauth` credential kind lets a provider authenticate against
-- OpenAI's ChatGPT subscription backend instead of the normal API. A provider
-- with this kind stores encrypted OAuth tokens (access + refresh) rather than
-- a static API key.
--
-- The credential_kind column needs a CHECK extension, so the existing
-- constraint is dropped and recreated. The token blob lives in a new column
-- because OAuth tokens are multiple fields that change at different times:
-- the refresh token is stable, the access token expires and is replaced.

-- Drop the old constraint, add the new one. PostgreSQL lets us do this in
-- one ALTER if we name the constraint, but since the original was inline
-- (unnamed constraint on the column), we recreate it.
ALTER TABLE providers
    DROP CONSTRAINT IF EXISTS providers_credential_kind_check,
    ADD CONSTRAINT credential_kind_chatgpt_oauth
        CHECK (credential_kind IN ('static', 'gcp_service_account', 'chatgpt_oauth'));

-- Encrypted OAuth tokens: {access_token, refresh_token, token_type,
-- expires_in, created_at}. Stored like upstream_api_key — AES-256-GCM,
-- decrypted only in the snapshot, never returned by the admin API.
ALTER TABLE providers
    ADD COLUMN chatgpt_oauth_tokens BYTEA DEFAULT NULL;

COMMENT ON COLUMN providers.chatgpt_oauth_tokens IS
    'Encrypted OAuth tokens for ChatGPT subscription auth. JSON: '
    'access_token, refresh_token, token_type, expires_in, created_at. '
    'Encrypted with the key in FASTLLM_ENCRYPTION_KEY. Never returned by '
    'the admin API. Carried decrypted in the snapshot.';

-- Provider catalogue entry for OpenAI ChatGPT.
-- The ChatGPT endpoint speaks OpenAI-compatible JSON, uses Authorization:
-- Bearer, and requires an OAuth credential_kind.
INSERT INTO provider_catalogue (key, display_name, base_url, protocol,
                                auth_header, auth_scheme, notes, credential_kinds)
VALUES (
    'openai',
    'OpenAI',
    'https://api.openai.com/v1',
    'openai',
    'authorization',
    'Bearer',
    NULL,
    'static,chatgpt_oauth'
) ON CONFLICT (key) DO UPDATE
    SET credential_kinds = EXCLUDED.credential_kinds;
