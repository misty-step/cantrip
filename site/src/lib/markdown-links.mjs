import { readFileSync } from 'node:fs';
import { dirname, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { docs, repositoryUrl } from './docs.mjs';

const root = resolve(process.cwd(), '..');
const routes = new Map(docs.map((doc) => [`docs/${doc.file}`, `/docs/${doc.slug}`]));

// Rewriting parsed HTML also covers Markdown reference links and explicit HTML
// links without changing code fences, heading IDs, or the canonical source.
export function canonicalLinks(context) {
  if (!context.fileURL) throw new Error('Markdown source URL is required for canonical links');
  const filename = fileURLToPath(context.fileURL);
  let source = relative(root, filename).split(sep).join('/');
  let revision = 'master';
  const published = /^site\/data\/releases\/(v\d+\.\d+\.\d+)\//.exec(source);
  if (published) {
    source = `docs/releases/${published[1]}.md`;
    revision = JSON.parse(readFileSync(resolve(dirname(filename), 'release.json'), 'utf8')).source_revision;
  }

  function destination(href, image) {
    if (!href || href.startsWith('#') || href.startsWith('?') || href.startsWith('//') || /^[a-z][a-z\d+.-]*:/i.test(href)) return href;
    // Root-relative links already name website routes, not repository files.
    if (href.startsWith('/')) return href;
    const url = new URL(href, `https://repository.invalid/${source}`);
    const path = decodeURIComponent(url.pathname.slice(1));
    const route = routes.get(path);
    if (route && !image) return `${route}${url.search}${url.hash}`;
    const encoded = path.split('/').map(encodeURIComponent).join('/');
    const base = image ? `https://raw.githubusercontent.com/misty-step/cantrip/${revision}` : `${repositoryUrl}/blob/${revision}`;
    return `${base}/${encoded}${url.search}${url.hash}`;
  }

  return {
    name: 'cantrip-canonical-links',
    element: {
      filter: ['a', 'img', 'pre'],
      visit(node, ctx) {
        if (node.tagName === 'pre') {
          ctx.setProperty(node, 'tabIndex', 0);
          return;
        }
        const image = node.tagName === 'img';
        const attribute = image ? 'src' : 'href';
        const value = node.properties[attribute];
        if (typeof value === 'string') ctx.setProperty(node, attribute, destination(value, image));
      },
    },
  };
}
