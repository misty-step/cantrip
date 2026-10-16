import { execFileSync } from 'node:child_process';

const revision = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (!/^[a-f0-9]{40}$/.test(revision)) throw new Error('Missing website source revision');

export const siteIdentity = {
  service: 'cantrip-site',
  environment: 'production',
  revision,
  release: `cantrip-site@${revision}`,
};
