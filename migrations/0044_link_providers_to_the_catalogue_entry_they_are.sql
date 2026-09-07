-- Say which catalogue entry an existing provider already is.
--
-- `catalogue_key` records that a provider was created by choosing a vendor
-- rather than typing an address, and it is the only thing that distinguishes
-- "we know this vendor's wire protocol" from "nobody has said". Providers that
-- predate the catalogue have it NULL even when they are, unmistakably, one of
-- its entries: the deployment's OpenRouter row was created by attaching a
-- backend to `https://openrouter.ai/api/v1`, which is exactly the address the
-- catalogue lists for OpenRouter.
--
-- Left alone, the Providers screen keeps asking those rows for a protocol it
-- already knows -- and offering the answer as a dropdown, which is an
-- invitation to set a vendor to something it does not speak.
--
-- Matched on the address alone, and only where it is exactly the catalogue's,
-- because that is the whole of the claim being made. A provider pointing at a
-- vendor's endpoint *is* that vendor. Anything else -- a region filled into a
-- placeholder, a self-hosted address, a gateway in front of a vendor -- does
-- not match and stays NULL, which is the honest answer for it.

UPDATE providers p
   SET catalogue_key = c.key
  FROM provider_catalogue c
 WHERE p.catalogue_key IS NULL
   AND p.api_base = c.base_url
   AND c.base_url NOT LIKE '%<%';
