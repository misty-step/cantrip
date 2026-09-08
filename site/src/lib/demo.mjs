import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

export function demoTranscript() {
  const filename = resolve(process.cwd(), 'public/demo/evidence.json');
  const evidence = JSON.parse(readFileSync(filename, 'utf8'));
  if (typeof evidence.transcript !== 'string' || evidence.transcript.trim().length === 0) {
    throw new Error('The real demonstration evidence must include its final editor transcript.');
  }
  return evidence.transcript;
}
