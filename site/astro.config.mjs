import { defineConfig } from 'astro/config';
import { satteri } from '@astrojs/markdown-satteri';
import { canonicalLinks } from './src/lib/markdown-links.mjs';
import { loadReleases } from './src/lib/releases.mjs';

// Rendering is deliberately offline. Missing or mismatched published inputs are
// an error, never permission to advertise an unreleased local Cargo version.
loadReleases();

export default defineConfig({
  site: 'https://cantrip.mistystep.io',
  output: 'static',
  trailingSlash: 'never',
  markdown: {
    processor: satteri({
      features: { smartPunctuation: false, rawHtml: true },
      hastPlugins: [canonicalLinks],
    }),
    shikiConfig: { theme: 'tokyo-night' },
  },
});
