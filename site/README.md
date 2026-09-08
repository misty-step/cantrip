# Cantrip website

Static Astro website for `https://cantrip.mistystep.io`, deployed with Cloudflare
Workers Static Assets. There is no SSR adapter, account system, client framework,
or build-time release fetch. IBM Plex fonts are bundled locally from the pinned
Fontsource packages; visitors do not contact a font CDN.

## Local commands

Use Node 24 LTS and npm. Node 26 is also within the supported local engine range.
Run **all commands from `site/`**; rendering resolves repository documents and
published data relative to this directory.

```sh
cd site
npm ci
npm run dev -- --host 127.0.0.1
```

Development defaults to `http://127.0.0.1:4321`. For static output:

```sh
npm run check
npm run build
npm run preview -- --host 127.0.0.1
```

`check` runs Astro/TypeScript diagnostics. `build` writes `site/dist/`, reading
only checked-out documents, media, fonts, and cached release inputs. Neither
command acquires release data or deploys. The repository's `scripts/check-site`
owns the integrated install/build and generated-link checks.

`package.json` pins direct dependencies, `package-lock.json` pins the dependency
tree, and `allowScripts` explicitly permits the pinned esbuild and workerd
installers on npm versions that require lifecycle-script approval. Review those
approvals when updating dependencies.

## Published releases, acquired separately

The checked-in `data/releases/` is a snapshot of actual published GitHub assets,
not a second manually maintained release history. Refresh it deliberately:

```sh
npm run sync:releases
```

This command needs the GitHub CLI (`gh`), network access, and GitHub authentication
(`gh auth login`, or `GH_TOKEN` in CI). It can run before `npm ci`: its imports are
only Node builtins and repository-owned JavaScript.

The command enumerates all published stable GitHub releases and asks GitHub for
the latest stable tag. For each tag it downloads `release.json`, `SHA256SUMS`,
`provenance.json`, `verification.json`, Landmark's `releases.json`, and the public
`vX.Y.Z.md` notes. It verifies:

- repository, version, target, download URLs, and source revision agreement;
- published asset identities and SHA-256 hashes for the manifest, notes, feed,
  and runtime proof;
- agreement between the Landmark Markdown and the public notes asset;
- a passed runtime proof for the same source revision and archive hash;
- the manifest's GitHub attestation against the release workflow, source digest,
  and GitHub-hosted runner requirement.

The acquisition does not download the large executable: the signed manifest
binds its published name and hash to the same source. The release pipeline owns
runtime artifact verification. Website rendering rechecks the cached identities
and hashes offline; missing or mismatched required inputs stop the build.

Only after every release passes does acquisition replace `data/releases/`.
Commit that whole directory with its updated `index.json`. Failed acquisition
leaves the previous snapshot intact and exits unsuccessfully; it never invents
fallback versions or notes. Local `Cargo.toml` and unpublished notes are not site
release inputs. Restart the dev server after refreshing a snapshot.

## Canonical documentation

`src/content.config.ts` loads these actual repository Markdown files:

| Repository source | Website route |
| --- | --- |
| `docs/INSTALLATION.md` | `/docs/install` |
| `docs/USAGE.md` | `/docs/use` |
| `docs/CONFIGURATION.md` | `/docs/configuration` |
| `docs/DESKTOP.md` | `/docs/desktop` |
| `docs/PRIVACY.md` | `/docs/privacy` |

Edit the repository source, not a site copy. `src/lib/docs.mjs` owns navigation
labels and routes, while Astro generates heading IDs and page contents. The
Markdown HTML-tree plugin rewrites local document links to these routes and
preserves fragments. Other relative repository references become explicit
GitHub source links; code fences are untouched. Published-note source references
are pinned to that release's source revision, not the current branch.

`/releases` and `/releases/vX.Y.Z` use the acquired data and exact published
Markdown. They never render the local unpublished `docs/releases/` files.

## Real demonstration media

`public/demo/` contains the actual native application capture, not a simulated
HUD. Its required interface is:

- `dictation.webm`: a 1280 × 800 real dictation session;
- `dictation-poster.png`: a still from that capture;
- `dictation.vtt`: English speech and phase captions;
- `evidence.json`: recording provenance, including the final editor text in a
  nonempty `transcript` string.

The homepage uses native playback controls, no autoplay, `preload="none"`, a
captions track, a still-image link, and a readable text alternative taken directly
from `evidence.transcript`. It links the recording provenance beside the video.
The screenshot is also the social-preview image. Do not substitute generated
waveforms or placeholder images when recording assets are missing.

## Deployment

After verification and a successful build, use the project-owned
`wrangler.jsonc` and the intended Cloudflare account:

```sh
npm run deploy
```

This uploads the existing static output; it does not build or refresh release
data. Cloudflare credentials belong in the operator environment or CI secrets,
never this tree. The website deployment workflow and Wrangler configuration own
the deployment policy and custom domain.

`.github/workflows/site.yml` checks pull requests and deploys after successful
`master` CI (including release publication), or on a deliberate manual dispatch.
It refreshes signed release inputs before checking and building. Configure the
repository secret `CLOUDFLARE_API_TOKEN` with Workers Scripts Edit for the Misty
Step account and Workers Routes Edit plus Zone Read for `mistystep.io`. Store it
with `gh secret set CLOUDFLARE_API_TOKEN --repo misty-step/cantrip`; do not commit
credentials or substitute the operator's broad OAuth session.

The custom-domain route in `wrangler.jsonc` lets Cloudflare manage DNS and TLS.
`workers.dev` and preview URLs are disabled. The workflow checks public docs
and release routes after deploying; `scripts/check-site` catches broken internal
pages, fragments, and assets before upload.
