import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { docs } from './lib/docs.mjs';

const documentation = defineCollection({
  loader: glob({
    base: '../docs',
    pattern: docs.map((doc) => doc.file),
    generateId: ({ entry }) => {
      const doc = docs.find((candidate) => candidate.file === entry);
      if (!doc) throw new Error(`Unknown canonical document: ${entry}`);
      return doc.slug;
    },
  }),
});

const releaseNotes = defineCollection({
  loader: glob({
    base: './data/releases',
    pattern: '*/v*.md',
    generateId: ({ entry }) => entry.split('/')[0]!,
  }),
});

export const collections = { documentation, releaseNotes };
