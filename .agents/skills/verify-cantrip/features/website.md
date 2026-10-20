# Website

A visitor reads <https://cantrip.mistystep.io>, follows the docs, changelog,
releases, and speech evals, and downloads the release. Operators check which
source revision is deployed.

## Sub-features

- `site-home` renders the hero, the current release tag, and the Linux download.
- `site-pages` serves `/docs/install`, `/docs/use`, `/changelog`, `/releases`, `/evals`.
- `site-links` every internal link on `/` returns 200; the download link resolves on GitHub.
- `site-identity` `/health.json` reports `ok`, `production`, and a revision equal to or behind master.
- `site-404` an unknown route returns 404.
- `site-mobile` the home page renders at 390×844 and 1440×900.

## How to get to it (user POV)

- Browse <https://cantrip.mistystep.io> and its nav: Documentation, Changelog, Speech evals, GitHub.

## Driving it with verify-cantrip

Preconditions:

- Network access; `gh` logged in (reads master's SHA and the latest release tag).

- **Journey.** `$V site --out "$RUN"`; read-only. Writes `evidence/site.json`,
  `site-desktop.png`, `site-mobile.png`.
- **Local build.** `python3 scripts/check-site` builds `site/` and checks local
  links; `scripts/verify-site` waits for the edge to serve a built page.
- **Proof.** Open both screenshots: the hero, "Download for Linux x86-64", and
  the release tag are visible, with no horizontal overflow on mobile.

## Gotchas

- Cloudflare returns 403 to Python's default user agent; the helper sends its own.
- `/docs/` has no index page; the nav links straight to `/docs/install`.
- A deployed revision behind master is normal between a merge and the deploy;
  a revision not in master's history is the failure.
- There is no account, form, or write path on the site.
