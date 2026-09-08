import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { repository } from '../src/lib/docs.mjs';
import { loadReleases, readRelease, releaseDirectory } from '../src/lib/releases.mjs';

function gh(...args) {
  return execFileSync('gh', args, {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'inherit'],
    timeout: 120_000,
  });
}

function saveJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

function download(tag, directory, names) {
  gh('release', 'download', tag, '--repo', repository, '--dir', directory,
    ...names.flatMap((name) => ['--pattern', name]));
}

async function sync() {
  if (process.argv.length !== 2) throw new Error('Usage: npm run sync:releases (syncs all published stable releases)');
  const releases = JSON.parse(gh('api', '--paginate', '--slurp', `repos/${repository}/releases?per_page=100`))
    .flat()
    .filter((release) => !release.draft && !release.prerelease);
  if (releases.length === 0) throw new Error('GitHub has no published stable Cantrip release. No website data was changed.');
  const latest = JSON.parse(gh('api', `repos/${repository}/releases/latest`)).tag_name;
  releases.sort((left, right) => Date.parse(right.published_at) - Date.parse(left.published_at) || right.tag_name.localeCompare(left.tag_name));
  mkdirSync(dirname(releaseDirectory), { recursive: true });
  const staging = mkdtempSync(join(dirname(releaseDirectory), '.releases-sync-'));
  const backup = `${staging}-previous`;
  try {
    for (const release of releases) {
      const tag = release.tag_name;
      if (!/^v\d+\.\d+\.\d+$/.test(tag)) throw new Error(`Unsupported stable release tag: ${tag}`);
      const directory = join(staging, tag);
      mkdirSync(directory);
      const assets = release.assets.map(({ id, name, size, digest, browser_download_url }) => ({
        id, name, size, digest, browser_download_url,
      })).sort((left, right) => left.name.localeCompare(right.name));
      saveJson(join(directory, 'github.json'), {
        id: release.id,
        tag_name: tag,
        html_url: release.html_url,
        draft: release.draft,
        prerelease: release.prerelease,
        published_at: release.published_at,
        assets,
      });
      download(tag, directory, ['release.json', 'SHA256SUMS', 'provenance.json', 'releases.json', 'verification.json', `${tag}.md`]);
      const record = readRelease(directory, tag);
      // The signed manifest binds the archive, public notes, Landmark feed, and
      // runtime proof. Downloading the large executable is not part of a site build.
      gh('attestation', 'verify', join(directory, 'release.json'),
        '--repo', repository,
        '--bundle', join(directory, 'provenance.json'),
        '--source-digest', record.sourceRevision,
        '--signer-workflow', `${repository}/.github/workflows/release.yml`,
        '--deny-self-hosted-runners');
      console.log(`Acquired ${tag}: signed source ${record.sourceRevision}`);
    }
    saveJson(join(staging, 'index.json'), {
      schema_version: 1,
      repository,
      latest,
      tags: releases.map((release) => release.tag_name),
    });
    loadReleases(staging);
    if (existsSync(releaseDirectory)) renameSync(releaseDirectory, backup);
    try {
      renameSync(staging, releaseDirectory);
    } catch (error) {
      if (existsSync(backup)) renameSync(backup, releaseDirectory);
      throw error;
    }
    rmSync(backup, { recursive: true, force: true });
    console.log(`Saved ${releases.length} published release(s); latest ${latest}. Commit site/data/releases with the site change.`);
  } finally {
    rmSync(staging, { recursive: true, force: true });
  }
}

sync().catch((error) => {
  console.error(`Release acquisition failed: ${error.message}`);
  process.exitCode = 1;
});
