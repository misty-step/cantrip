import { siteIdentity } from '../lib/site-identity.mjs';

// Static artifact identity, not a desktop/evaluation readiness claim.
export function GET() {
  return new Response(JSON.stringify({ status: 'ok', ...siteIdentity }) + '\n', {
    headers: { 'Content-Type': 'application/json' },
  });
}
