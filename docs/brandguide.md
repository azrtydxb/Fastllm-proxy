FastLLM Proxy — Brand & UI Guide

This document defines the visual identity for FastLLM Proxy and should be treated as the source of truth when implementing the website, documentation, dashboard, GitHub assets, and other product interfaces.

The visual language should communicate:

Speed · Intelligence · Routing · Efficiency · Infrastructure

The brand should feel like a modern developer/infrastructure product, not a generic AI application.

⸻

1. Brand Identity

Product name

FastLLM Proxy

Preferred presentation:

FastLLM
PROXY

Do not rename the product to:

- Fast LLM
- FastLLMProxy
- Fast LLM Proxy
- FLLM

In normal written copy, use FastLLM Proxy.

⸻

2. Core Brand Concept

The logo combines three concepts:

Data flow

The horizontal lines represent requests/tokens flowing through the proxy.

Intelligent routing

The central arrow represents requests being routed toward the optimal destination/provider.

Speed

The trailing lines and neon glow create a sense of acceleration.

The hexagonal enclosure represents infrastructure, APIs, networking, and a controlled routing layer.

The UI should reinforce these concepts with subtle:

- data-flow lines
- routing paths
- gradients
- nodes
- network patterns
- hexagonal geometry
- restrained glow effects

Do not turn the entire interface into a cyberpunk dashboard. The branding can be visually energetic while the application UI remains clean and highly usable.

⸻

3. Logo Assets

There are three primary logo variants.

A. Primary Logo

The full horizontal logo containing:

Icon + FastLLM + PROXY + tagline

Use for:

- website navigation/hero areas
- About page
- documentation landing pages
- login screen
- marketing pages
- presentations
- social previews

Do not use the full logo when available space makes the tagline difficult to read.

⸻

B. Icon / Favicon

The standalone hexagonal routing icon.

Use for:

- favicon
- browser tab
- app icon
- PWA icon
- GitHub organization/avatar
- compact sidebar
- loading screen
- mobile navigation
- social avatar

When displayed at small sizes, prefer the icon without surrounding text.

Recommended favicon sizes:

16×16
32×32
48×48
180×180 Apple Touch
192×192 PWA
512×512 PWA / high resolution

For very small sizes, preserve the overall hexagon/arrow silhouette rather than tiny details.

⸻

C. GitHub / README Banner

The wide banner is specifically intended for:

- GitHub README
- repository landing page
- documentation hero
- project announcements
- social sharing

Place it near the beginning of the README.

Avoid putting additional headings or text over the image.

⸻

4. Primary Color Palette

The FastLLM identity is based around a transition from violet → electric blue → cyan.

Fast Violet

--fast-violet: #8B20FF;

Use for:

- intelligent-routing accents
- active indicators
- gradient origins
- selected states
- decorative glow

⸻

Electric Purple

--electric-purple: #6726FF;

Use as a transition between violet and blue.

⸻

Fast Blue

--fast-blue: #1769FF;

This is the main functional brand color.

Use for:

- primary actions
- links
- charts
- routing indicators
- focus states
- active navigation

⸻

Electric Cyan

--fast-cyan: #00D9F5;

Use for:

- successful routing
- optimized states
- highlights
- performance indicators
- gradient endpoints

Cyan should normally be an accent, not the dominant page color.

⸻

5. Brand Gradient

The signature FastLLM gradient is:

background: linear-gradient(
90deg,
#8B20FF 0%,
#6726FF 25%,
#1769FF 60%,
#00D9F5 100%
);

Create a reusable design token:

--gradient-fastllm:
linear-gradient(
90deg,
#8B20FF 0%,
#6726FF 25%,
#1769FF 60%,
#00D9F5 100%
);

Use this gradient for:

- FastLLM wordmark accents
- primary hero elements
- important CTA borders
- active routing visualization
- loading indicators
- selected metric highlights
- occasional headline text

Do not apply the gradient to large amounts of body text.

⸻

6. Dark UI Palette

FastLLM should be dark-first.

The primary application background should not be pure black.

Background

--bg-primary: #030817;

Elevated Background

--bg-secondary: #071126;

Cards

--bg-card: #0A1530;

Elevated Cards

--bg-elevated: #0D1B38;

Borders

--border-subtle: #17254A;

Strong Borders

--border-strong: #253A6B;

These blue-black tones keep the interface visually connected to the logo.

⸻

7. Text Colors

Primary

--text-primary: #F8FAFF;

Use for:

- headings
- important values
- primary content

Secondary

--text-secondary: #AAB7D1;

Use for:

- descriptions
- labels
- secondary content

Muted

--text-muted: #687897;

Use for:

- timestamps
- hints
- inactive elements
- secondary metadata

Avoid pure gray wherever possible. FastLLM neutrals should contain a subtle blue tint.

⸻

8. Semantic Colors

Do not use the brand gradient for every application state.

Maintain clear semantic colors.

--success: #20D997;
--warning: #F5B942;
--danger: #FF5570;
--info: #23B7F5;

Examples:

Provider online

● Online

Use success green.

Provider latency warning

Use warning amber.

Provider failure

Use danger red.

Optimized / cached

Cyan can be used because optimization is part of the FastLLM brand vocabulary.

⸻

9. Recommended Complete Token Set

The AI coder should create centralized tokens rather than hard-coding colors throughout components.

:root {
/* Brand _/
--fast-violet: #8B20FF;
--electric-purple: #6726FF;
--fast-blue: #1769FF;
--fast-cyan: #00D9F5;
/_ Background _/
--bg-primary: #030817;
--bg-secondary: #071126;
--bg-card: #0A1530;
--bg-elevated: #0D1B38;
/_ Borders _/
--border-subtle: #17254A;
--border-strong: #253A6B;
/_ Text _/
--text-primary: #F8FAFF;
--text-secondary: #AAB7D1;
--text-muted: #687897;
/_ Semantic _/
--success: #20D997;
--warning: #F5B942;
--danger: #FF5570;
--info: #23B7F5;
/_ Brand gradient */
--gradient-fastllm:
linear-gradient(
90deg,
#8B20FF 0%,
#6726FF 25%,
#1769FF 60%,
#00D9F5 100%
);
}

All application components should consume the design system rather than introducing arbitrary colors.

⸻

10. Typography

FastLLM should use modern geometric sans-serif typography.

Preferred:

Inter

Alternative:

Geist

Both work extremely well for developer tooling and dashboards.

Recommended stack:

font-family:
Inter,
Geist,
-apple-system,
BlinkMacSystemFont,
"Segoe UI",
sans-serif;

For technical data use:

JetBrains Mono

or

Geist Mono

Use monospace for:

- API keys
- model names
- request IDs
- endpoints
- tokens
- latency
- logs
- JSON
- code

Example:

openai/gpt-5
anthropic/claude-sonnet
34 ms
12,482 tokens
$0.0142

⸻

11. Typography Hierarchy

Hero

48–72px
700–800 weight

H1

36–48px
700

H2

28–32px
600–700

H3

20–24px
600

Body

14–16px
400

Labels

12–14px
500–600

Dashboard typography should remain compact and information-dense.

⸻

12. UI Design Philosophy

The dashboard should feel like a combination of:

developer infrastructure + observability + AI routing

It should NOT resemble:

- a crypto application
- a gaming UI
- a generic ChatGPT clone
- an overly glowing cyberpunk interface

Think:

clean infrastructure UI with restrained futuristic accents.

Approximately:

90% clean interface

10% neon branding

⸻

13. Cards

Cards should use dark navy surfaces.

Example:

.fast-card {
background: #0A1530;
border: 1px solid #17254A;
border-radius: 12px;
}

On hover:

.fast-card:hover {
border-color: #253A6B;
}

Important cards may receive a subtle blue glow:

box-shadow:
0 0 24px rgba(23, 105, 255, 0.08);

Keep glow subtle.

⸻

14. Buttons

Primary

Use blue or the FastLLM gradient.

background: var(--gradient-fastllm);
color: white;

Typical actions:

- Add Provider
- Create Route
- Save Configuration
- Create API Key

Secondary

background: #0D1B38;
border: 1px solid #253A6B;
color: #F8FAFF;

Destructive

Always use the danger color rather than purple.

⸻

15. Border Radius

Use moderately rounded geometry.

Buttons: 8px
Inputs: 8px
Cards: 12px
Dialogs: 16px
Large panels: 16px

Avoid excessive pill-shaped UI.

Pills are appropriate for:

- status
- model labels
- provider labels
- tags

⸻

16. Glow Effects

Glow is part of the brand but should be controlled.

Recommended:

box-shadow:
0 0 20px rgba(0, 217, 245, 0.12);

or

box-shadow:
0 0 24px rgba(139, 32, 255, 0.12);

Strong neon glow should mainly appear in:

- marketing pages
- hero areas
- loading screens
- routing visualizations

Do not surround every dashboard component with neon.

⸻

17. Dashboard Visual Language

The dashboard should visually communicate requests flowing:

Application
↓
FastLLM
↓
Routing
↙ ↓ ↘
OpenAI Anthropic Local

Routing visualizations can use the brand gradient.

For example:

incoming request

Purple

↓

FastLLM processing

Blue

↓

optimized/routed request

Cyan

This creates a visual meaning for the brand gradient rather than using it decoratively.

⸻

18. Metrics

Important metrics should be immediately readable.

Examples:

Requests
1.24M
Cache Hit Rate
67.4%
Tokens Saved
42.8M
Average Latency
38 ms
Cost Saved
$1,284

Values should be visually dominant.

Labels should use secondary text.

Use cyan or blue sparingly to emphasize positive optimization metrics.

⸻

19. Charts

Charts should use the brand palette.

Recommended series order:

#1769FF
#00D9F5
#8B20FF
#6726FF

Semantic events should override brand colors.

Errors:

#FF5570

Warnings:

#F5B942

Success:

#20D997

Chart backgrounds should remain transparent or match card surfaces.

Grid lines should use:

#17254A

⸻

20. Provider Identity

Provider logos should retain their official branding.

Examples could include providers such as:

OpenAI
Anthropic
Google
Mistral
Groq
OpenRouter
Azure
AWS
Local / vLLM

Do not recolor provider logos into the FastLLM gradient.

FastLLM branding should surround provider identity rather than replace it.

⸻

21. Icons

Use one consistent icon library.

Preferred:

Lucide

Use line icons with approximately:

1.5–2px stroke

Typical mappings:

Routing → Route
Providers → Network
Caching → Database
Performance → Gauge
Cost → CircleDollarSign
Requests → Activity
Models → Brain
API Keys → Key
Logs → ScrollText
Settings → Settings

Do not mix multiple unrelated icon styles.

⸻

22. Navigation

Recommended sidebar:

[ FastLLM icon ]
Overview
Routing
Providers
Models
Requests
Cache
Analytics
API Keys
Settings

The selected item can use:

background: rgba(23,105,255,0.12);
color: #F8FAFF;

with a blue/cyan indicator.

⸻

23. Inputs

Inputs should be understated.

background: #071126;
border: 1px solid #17254A;
color: #F8FAFF;

Focused:

border-color: #1769FF;
box-shadow:
0 0 0 3px rgba(23,105,255,0.15);

Never use large neon glows around form fields.

⸻

24. Tables

FastLLM will likely contain significant operational data, so tables should prioritize readability.

Use:

dark background
subtle row separators
compact spacing
monospace technical values
clear status indicators

Example:

Provider Model Latency Tokens Cost Status
OpenAI gpt-* 38ms 1,842 $0.014	●
Anthropic	claude-*	44ms	1,731	$0.012 ●
Local qwen-* 21ms 2,103 $0.003 ●

Avoid heavy borders around every cell.

⸻

25. Background Decoration

Marketing pages may use:

- subtle hexagonal grids
- flowing data lines
- blurred gradient orbs
- network nodes
- light trails

Example background glow:

background:
radial-gradient(
circle at 20% 20%,
rgba(103,38,255,.12),
transparent 35%
),
radial-gradient(
circle at 80% 30%,
rgba(0,217,245,.08),
transparent 35%
),
#030817;

Dashboard pages should use significantly less decoration.

⸻

26. Motion

Animations should reinforce speed and routing.

Good:

- data moving along paths
- subtle gradient movement
- request pulses
- routing-node activation
- fast card transitions
- number/count animations
- subtle loading streaks

Avoid:

- bouncing UI
- excessive floating elements
- long transitions
- large parallax effects

Recommended UI transition:

transition: 150ms ease;

The product is called FastLLM. The UI should therefore feel immediate.

⸻

27. Loading State

Avoid generic spinning loaders when possible.

A branded loader can animate:

────●────→

or animate the three input lines of the FastLLM icon toward the arrow.

The animation should suggest:

request → processing → routing

⸻

28. Light Mode

Dark mode is the canonical FastLLM identity.

If light mode is implemented, preserve:

- blue
- violet
- cyan
- dark navy typography

Do not redesign the brand around pastel colors.

Suggested light background:

#F6F8FC

Cards:

#FFFFFF

Text:

#071126

Borders:

#DCE4F2

The dark theme should remain the default visual reference.

⸻

29. Logo Rules

Always:

- preserve aspect ratio
- maintain clear space around the logo
- use supplied logo files
- keep the gradient intact
- use the icon when space is constrained

Never:

- stretch the logo
- rotate it
- recolor it randomly
- add another gradient
- add drop shadows unrelated to the original design
- place it over visually noisy content
- separate parts of the icon
- recreate the logo using an icon library

⸻

30. Clear Space

Maintain approximately 20% of the logo height as minimum clear space around the logo.

For the standalone icon, use approximately:

10–15% internal padding

when used as an application icon.

⸻

31. README Usage

Recommended README structure:

[BANNER]
FastLLM Proxy
Short product description
Badges
Why FastLLM?
Features
Architecture
Quick Start
Configuration
Providers
Routing
Caching / Optimization
Observability
Benchmarks
Documentation
Contributing
License

Because the banner already contains the logo and product identity, avoid immediately repeating another giant logo underneath it.

⸻

32. Marketing Tone

The visual
should be concise and technical.

Preferred messaging:

Faster. Smarter. Cheaper.

Supporting themes:

Route intelligently.
Choose the right model/provider for each request.

Reduce latency.
Cache and optimize wherever possible.

Reduce cost.
Avoid spending tokens and compute unnecessarily.

Stay provider-independent.
Applications integrate with FastLLM rather than individual LLM providers.

Avoid vague AI marketing language such as:

- revolutionary AI
- unlock the power of AI
- next-generation intelligence
- transform your AI journey

FastLLM should sound like serious infrastructure software.

⸻

33. Overall Visual Reference

When creating a new FastLLM page or component, ask:

Does this look like a high-performance piece of infrastructure that happens to route AI workloads?

The answer should be yes.

The visual hierarchy should generally be:

Dark Navy Foundation
↓
Clean Functional UI
↓
Electric Blue Interaction
↓
Violet → Blue → Cyan Brand Accents
↓
Subtle Glow / Data-flow Effects

Not:

Neon everywhere

- gradients everywhere
- glowing borders everywhere

The logo is intentionally expressive. The application surrounding it should give it room to stand out.

⸻

34. AI Coder Implementation Rules

When implementing or modifying FastLLM Proxy, follow these rules:

1. Use this brand guide as the design-system source of truth.
2. Use the supplied logo assets. Never recreate the logo.
3. Default to the dark FastLLM theme.
4. Store colors as centralized design tokens/theme variables.
5. Never introduce arbitrary purple, blue, cyan, gray, or background colors when an existing token is appropriate.
6. Use the violet → blue → cyan gradient only for high-value brand elements.
7. Use Fast Blue #1769FF as the primary functional interaction color.
8. Use semantic green, amber, and red for success/warning/error states rather than forcing brand colors.
9. Keep dashboard surfaces clean and restrained.
10. Reserve strong neon effects for marketing areas, routing visualizations, and branded loading states.
11. Use Inter/Geist for UI typography and JetBrains Mono/Geist Mono for technical values.
12. Prefer Lucide for UI icons.
13. Maintain WCAG-readable contrast for all functional text.
14. All components must look coherent in the overall FastLLM design system.
15. Before adding a new visual treatment, determine whether an existing token/component already solves the requirement.
16. Reuse shared components for buttons, cards, inputs, badges, dialogs, tables, tooltips, and navigation.
17. Avoid inline styling and duplicated color definitions.
18. Keep animations short and purposeful.
19. Make responsive behavior part of the component implementation rather than an afterthought.
20. Functionality and readability always take precedence over decorative branding.

Core design instruction

Build FastLLM Proxy as a clean, premium developer-infrastructure product using a dark navy foundation and restrained violet → electric blue → cyan accents derived directly from the FastLLM logo. The logo is visually expressive; the application UI should be cleaner and quieter. Use neon/glow primarily to communicate routing, activity, optimization, and speed—not as decoration.

This rule should guide any UI decision not explicitly covered elsewhere in this document.

