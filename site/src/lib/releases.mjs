import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { repository, repositoryUrl } from './docs.mjs';

export const releaseDirectory = resolve(process.cwd(), 'data/releases');
const tagPattern = /^v\d+\.\d+\.\d+$/;
const hashPattern = /^[a-f0-9]{64}$/;

function requireValue(condition, message) {
  if (!condition) throw new Error(`Published release data: ${message}`);
}

function bytes(directory, name) {
  requireValue(typeof name === 'string' && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name), 'unsafe asset name');
  try {
    return readFileSync(join(directory, name));
  } catch (error) {
    throw new Error(`Missing published release input ${join(directory, name)}. Run npm run sync:releases before building.`, { cause: error });
  }
}

function json(directory, name) {
  return JSON.parse(bytes(directory, name).toString('utf8'));
}

function digest(content) {
  return createHash('sha256').update(content).digest('hex');
}

export function readRelease(directory, tag) {
  requireValue(tagPattern.test(tag), `unsupported stable tag ${tag}`);
  const manifestBytes = bytes(directory, 'release.json');
  const manifest = JSON.parse(manifestBytes.toString('utf8'));
  const github = json(directory, 'github.json');
  const releaseUrl = `${repositoryUrl}/releases/tag/${tag}`;
  const downloadBase = `${repositoryUrl}/releases/download/${tag}/`;

  requireValue(manifest.schema_version === 1 && manifest.repository === repository, `${tag}: unknown manifest schema or repository`);
  requireValue(manifest.tag === tag && `v${manifest.version}` === tag, `${tag}: manifest version mismatch`);
  requireValue(/^[a-f0-9]{40}$/.test(manifest.source_revision), `${tag}: invalid source revision`);
  requireValue(manifest.release_url === releaseUrl && manifest.download_base_url === downloadBase, `${tag}: download origin mismatch`);
  requireValue(manifest.target === 'x86_64-unknown-linux-gnu', `${tag}: unsupported download target`);
  requireValue(typeof manifest.runtime_baseline?.distribution === 'string' && /^\d+\.\d+$/.test(manifest.runtime_baseline?.glibc_min), `${tag}: missing runtime baseline`);
  requireValue(github.tag_name === tag && github.html_url === releaseUrl && github.draft === false && github.prerelease === false, `${tag}: not a published stable GitHub release`);
  requireValue(typeof github.published_at === 'string' && Number.isFinite(Date.parse(github.published_at)), `${tag}: missing publication date`);
  requireValue(Array.isArray(github.assets), `${tag}: missing published asset list`);

  const checksums = new Map();
  for (const line of bytes(directory, 'SHA256SUMS').toString('utf8').trim().split('\n')) {
    const match = /^([a-f0-9]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)$/.exec(line);
    requireValue(match && !checksums.has(match[2]), `${tag}: invalid or duplicate checksum entry`);
    checksums.set(match[2], match[1]);
  }

  function asset(name, sha256) {
    requireValue(typeof name === 'string' && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(name), `${tag}: unsafe asset name`);
    const published = github.assets.filter((candidate) => candidate.name === name);
    requireValue(published.length === 1 && published[0].browser_download_url === `${downloadBase}${name}`, `${tag}: missing or misdirected published asset ${name}`);
    if (sha256 !== undefined) {
      requireValue(hashPattern.test(sha256) && checksums.get(name) === sha256, `${tag}: checksum mismatch for ${name}`);
      requireValue(!published[0].digest || published[0].digest === `sha256:${sha256}`, `${tag}: GitHub asset digest mismatch for ${name}`);
    }
    return published[0].browser_download_url;
  }

  function verifiedFile(descriptor) {
    requireValue(descriptor && hashPattern.test(descriptor.sha256), `${tag}: missing asset hash`);
    asset(descriptor.name, descriptor.sha256);
    const content = bytes(directory, descriptor.name);
    requireValue(digest(content) === descriptor.sha256, `${tag}: downloaded bytes differ for ${descriptor.name}`);
    return content;
  }

  asset('release.json', digest(manifestBytes));
  asset('SHA256SUMS');
  requireValue(manifest.archive?.name === `cantrip-${tag}-${manifest.target}.tar.gz`, `${tag}: archive identity mismatch`);
  const downloadUrl = asset(manifest.archive.name, manifest.archive.sha256);
  const notes = verifiedFile(manifest.public_notes?.markdown).toString('utf8');
  requireValue(manifest.public_notes.markdown.name === `${tag}.md` && notes.trim().length > 0, `${tag}: missing versioned public notes`);
  requireValue(manifest.landmark_feed?.name === 'releases.json', `${tag}: missing Landmark feed`);
  const feed = JSON.parse(verifiedFile(manifest.landmark_feed).toString('utf8'));
  requireValue(Array.isArray(feed), `${tag}: invalid Landmark feed`);
  const entries = feed.filter((entry) => entry.version === manifest.version);
  requireValue(entries.length === 1, `${tag}: Landmark version mismatch`);
  const landmark = entries[0];
  requireValue(landmark.schema_version === 'landmark.public-release-notes.v1' && landmark.repository === repository && landmark.release_url === releaseUrl, `${tag}: Landmark source mismatch`);
  requireValue(landmark.markdown === notes && (landmark.tag === manifest.version || landmark.tag === tag), `${tag}: Landmark notes differ from published Markdown`);
  requireValue(notes.includes(manifest.source_revision) && notes.includes(manifest.archive.sha256), `${tag}: public notes are not bound to the source and archive`);
  requireValue(Array.isArray(landmark.sections) && typeof landmark.sections[0]?.title === 'string', `${tag}: missing public release title`);

  const verification = JSON.parse(verifiedFile(manifest.verification).toString('utf8'));
  requireValue(verification.status === 'passed' && verification.source_revision === manifest.source_revision && verification.archive_sha256 === manifest.archive.sha256, `${tag}: runtime proof does not match source and archive`);
  requireValue(manifest.provenance?.name === 'provenance.json' && manifest.provenance.source_revision === manifest.source_revision && manifest.provenance.signer_workflow === `${repository}/.github/workflows/release.yml`, `${tag}: provenance identity mismatch`);
  asset(manifest.provenance.name);
  bytes(directory, manifest.provenance.name);
  asset(manifest.technical_changelog?.name, manifest.technical_changelog?.sha256);

  return {
    tag,
    version: manifest.version,
    title: landmark.sections[0].title,
    publishedAt: github.published_at,
    sourceRevision: manifest.source_revision,
    sourceUrl: `${repositoryUrl}/tree/${manifest.source_revision}`,
    target: manifest.target,
    baseline: manifest.runtime_baseline,
    archive: manifest.archive,
    downloadUrl,
    releaseUrl,
    notes,
    assets: {
      manifest: `${downloadBase}release.json`,
      checksums: `${downloadBase}SHA256SUMS`,
      provenance: `${downloadBase}${manifest.provenance.name}`,
      verification: `${downloadBase}${manifest.verification.name}`,
      markdown: `${downloadBase}${manifest.public_notes.markdown.name}`,
      landmark: `${downloadBase}${manifest.landmark_feed.name}`,
      technical: `${downloadBase}${manifest.technical_changelog.name}`,
    },
  };
}

export function loadReleases(directory = releaseDirectory) {
  const index = json(directory, 'index.json');
  requireValue(index.schema_version === 1 && index.repository === repository, 'unknown release index schema');
  requireValue(Array.isArray(index.tags) && index.tags.length > 0 && new Set(index.tags).size === index.tags.length, 'no unique published releases; run npm run sync:releases');
  requireValue(index.tags.includes(index.latest), 'latest published release is absent from the index');
  /** @type {ReturnType<typeof readRelease>[]} */
  const releases = index.tags.map((tag) => readRelease(join(directory, tag), tag));
  releases.sort((left, right) => Date.parse(right.publishedAt) - Date.parse(left.publishedAt) || right.tag.localeCompare(left.tag));
  return { releases, latest: releases.find((release) => release.tag === index.latest) };
}

export function releaseDate(value) {
  return new Intl.DateTimeFormat('en', { day: 'numeric', month: 'long', year: 'numeric', timeZone: 'UTC' }).format(new Date(value));
}
